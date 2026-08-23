//! [`FerrofinStreamStatePlanner`] — the concrete [`StreamStatePlanner`].
//!
//! This is the last large slice of the transcode port: turning a raw
//! [`HlsStreamRequest`] into a concrete [`TranscodePlan`] (the media-source
//! resolution, the [`EncodingJobInfo`] state, and the full ffmpeg HLS command
//! line). It is Jellyfin's `StreamingHelpers.GetStreamingState` +
//! `EncodingHelper.GetCommandLineArguments`, deliberately left behind the
//! [`StreamStatePlanner`] seam so the [`HlsStreamManagerImpl`] orchestration above
//! it stays testable.
//!
//! It lives in the composition-root binary (`ferrofin-server`) because it is the
//! one place that may depend on **both** `ferrofin-core`'s
//! [`MediaSourceManager`](ferrofin_traits::library::MediaSourceManager) **and**
//! `ferrofin-mediaencoding`'s [`EncodingHelper`] arg builder — the seam
//! ([`HlsStreamManagerImpl`]) lives in `ferrofin-hls`, which must not depend on
//! `ferrofin-core` (`RULES_CODE_REUSE`), so the concrete planner is injected from
//! above through the trait.
//!
//! # First-Light scope
//!
//! The planner honours the request-declared target codecs and **copies when it
//! can** (the cheapest correct path): [`EncodingHelper::can_stream_copy_video`] /
//! [`EncodingHelper::can_stream_copy_audio`] decide per stream, and a copyable
//! stream becomes a `-c:v copy` / `-c:a copy` remux. When it must re-encode, it
//! uses **NVENC** hardware encoding (h264/hevc/av1 + NVDEC decode) if the
//! persisted encoding options select it, else software `libx264`. What it does
//! *not* do is the full device-profile negotiation, the rest of the
//! hardware-acceleration matrix (QSV/VAAPI/AMF), or subtitle provider fan-out
//! (only stored/embedded burn-in). Those are deferred.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_core::FerrofinServerApplicationPaths;
use ferrofin_hls::{StreamStatePlanner, TranscodePlan};
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingHelper, EncodingJobInfo, ProbedEncoders,
};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{EncoderPreset, HardwareAccelerationType, MediaStreamType};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::MediaSourceManager;
use ferrofin_traits::media_encoding::{
    HlsStreamRequest, MediaEncoder, SubtitleEncoder, TranscodingJobType,
};
use ferrofin_traits::system::ServerApplicationPaths as _;
use uuid::Uuid;

/// The default HLS segment length, in seconds.
///
/// Matches Jellyfin's `StreamState.SegmentLength` default of `3` for H.264/HEVC
/// output: the first segment file can't close (and playback can't begin) until
/// this many seconds are encoded, so it directly sets time-to-first-segment.
/// Used when the request declares no `SegmentLength`.
const DEFAULT_SEGMENT_LENGTH_SECS: i32 = 3;

/// Assumed output frame rate for the NVENC GOP keyframe calc when the source
/// frame rate is unknown (probing returned none). Only fires on pathological
/// sources; `25` is a safe PAL-ish default. Candidate setting.
const DEFAULT_KEYFRAME_FPS: f32 = 25.0;

/// The default HLS segment container when the request declares none.
///
/// Port of the `DynamicHlsController` fallback: MPEG-TS (`ts`) segments.
const DEFAULT_SEGMENT_CONTAINER: &str = "ts";

/// The default output video codec when the request declares none.
///
/// Port of `StreamingHelpers`' default of `h264` for a video HLS stream — the
/// most broadly compatible target. Maps to `libx264` in [`EncodingHelper`].
const DEFAULT_VIDEO_CODEC: &str = "h264";

/// The default output audio codec when the request declares none.
///
/// Port of the `aac` default the audio HLS path uses — the most broadly
/// compatible target.
const DEFAULT_AUDIO_CODEC: &str = "aac";

/// The default encoder preset handed to [`EncodingHelper::video_quality_param`]
/// when the configured preset is `auto`. Port of the `veryfast` default the
/// software path uses for on-the-fly encode.
const DEFAULT_ENCODER_PRESET: EncoderPreset = EncoderPreset::veryfast;

/// The number of ffmpeg ticks per second (100 ns units). Port of
/// `TimeSpan.TicksPerSecond`.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The number of milliseconds per second.
const MS_PER_SECOND: i32 = 1000;

/// The concrete [`StreamStatePlanner`]: resolves a request into a transcode plan.
///
/// Holds the collaborators the resolution + arg-building needs: the
/// [`MediaSourceManager`] (item → [`MediaSourceInfo`]), the [`MediaEncoder`]
/// (input-argument + seek-time formatting), the [`EncodingHelper`] (encoder
/// selection, mapping, bitrate/quality/thread params, copy decision), the
/// [`ServerConfigurationManager`] (persisted encoding options), and the
/// application [`paths`](FerrofinServerApplicationPaths) (transcode cache root).
pub struct FerrofinStreamStatePlanner {
    media_sources: Arc<dyn MediaSourceManager>,
    encoder: Arc<dyn MediaEncoder>,
    encoding_helper: EncodingHelper<ProbedEncoders>,
    /// The server config manager — read for the persisted `encoding` options
    /// (hardware-acceleration type, presets) on each plan.
    config: Arc<dyn ServerConfigurationManager>,
    paths: Arc<FerrofinServerApplicationPaths>,
    /// The subtitle encoder — resolves a burned **text** subtitle to its small
    /// extracted/external file (cached across requests), so the `subtitles`
    /// filter doesn't re-demux the whole media file on every ffmpeg (re)start.
    subtitles: Arc<dyn SubtitleEncoder>,
    /// Whether the discovered ffmpeg reports the `tonemapx` filter
    /// (jellyfin-ffmpeg builds only). Selects the fast single-pass software
    /// HDR→SDR tonemap over the vanilla zscale chain; probed once at startup.
    supports_tonemapx: bool,
    /// Resolves item/series/library display names onto the plan's
    /// [`TranscodeDisplayNames`] so transcode logs name what they play.
    /// Optional: `None` (the composition unit test) leaves the names empty —
    /// they are logging metadata, never load-bearing.
    library: Option<Arc<dyn ferrofin_traits::library::LibraryManager>>,
}

impl FerrofinStreamStatePlanner {
    /// Assembles the planner from its collaborators.
    ///
    /// * `media_sources` — resolves an item id into its [`MediaSourceInfo`].
    /// * `encoder` — formats the ffmpeg input argument and the `-ss` seek time.
    /// * `encoding_helper` — builds the encoder/map/bitrate/quality/thread args
    ///   and the stream-copy decision (its [`ProbedEncoders`] carrying the
    ///   `-encoders` probe, so e.g. `libfdk_aac` is preferred when present).
    /// * `config` — the server configuration (persisted encoding options).
    /// * `paths` — the application paths (the transcode cache root).
    /// * `subtitles` — resolves a burned text subtitle to its cached file.
    /// * `supports_tonemapx` — whether the discovered ffmpeg has `tonemapx`
    ///   (from the startup `-filters` probe).
    #[must_use]
    pub fn new(
        media_sources: Arc<dyn MediaSourceManager>,
        encoder: Arc<dyn MediaEncoder>,
        encoding_helper: EncodingHelper<ProbedEncoders>,
        config: Arc<dyn ServerConfigurationManager>,
        paths: Arc<FerrofinServerApplicationPaths>,
        subtitles: Arc<dyn SubtitleEncoder>,
        supports_tonemapx: bool,
    ) -> Self {
        Self {
            media_sources,
            encoder,
            encoding_helper,
            config,
            paths,
            subtitles,
            supports_tonemapx,
            library: None,
        }
    }

    /// Wires the library seam so transcode logs carry item/series/library
    /// display names (the composition root calls this; unit tests skip it).
    #[must_use]
    pub fn with_library(
        mut self,
        library: Arc<dyn ferrofin_traits::library::LibraryManager>,
    ) -> Self {
        self.library = Some(library);
        self
    }

    /// Resolves the human-readable names for the playing item — its own title,
    /// its series (episodes), and its library — for the transcode logs.
    /// Best-effort by design: any miss leaves the field empty.
    async fn display_names(
        &self,
        item_id: uuid::Uuid,
    ) -> ferrofin_mediaencoding::TranscodeDisplayNames {
        let mut names = ferrofin_mediaencoding::TranscodeDisplayNames::default();
        let Some(library) = &self.library else {
            return names;
        };
        let Ok(Some(item)) = library.get_item_by_id(item_id).await else {
            return names;
        };
        names.item_name = item.name.clone();
        names.series_name = item.series_name.clone();
        if let Some(top) = item
            .top_parent_id
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            && let Ok(Some(folder)) = library.get_item_by_id(top).await
        {
            names.library_name = folder.name;
        }
        names
    }

    /// The effective [`EncodingOptions`] for this planner, read from the persisted
    /// named `encoding` config (falling back to [`EncodingOptions::default`] when
    /// unset or unreadable). This is what carries the user's hardware-acceleration
    /// choice (e.g. NVENC) into the transcode arg builder.
    async fn encoding_options(&self) -> EncodingOptions {
        self.config.get_encoding_options().await.unwrap_or_default()
    }

    /// Resolves the [`MediaSourceInfo`] for `request`.
    ///
    /// Port of the media-source resolution in `StreamingHelpers.GetStreamingState`:
    /// fetch the item's static media sources and select the one matching the
    /// request's `media_source_id` (defaulting to the first when unspecified).
    async fn resolve_media_source(
        &self,
        request: &HlsStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let sources = self
            .media_sources
            .get_static_media_sources(request.item_id, false, None)
            .await?;

        // `StreamingHelpers.GetStreamingState`: match the requested id, else when the
        // "media source id" is really the item id, the first source.
        let chosen = match request.media_source_id.as_deref() {
            Some(id) => match sources.iter().position(|s| s.id_matches(id)) {
                Some(i) => sources.into_iter().nth(i),
                None if Uuid::parse_str(id).is_ok_and(|g| g == request.item_id) => {
                    sources.into_iter().next()
                }
                None => None,
            },
            None => sources.into_iter().next(),
        };

        chosen.ok_or_else(|| {
            ServiceError::NotFound(format!("no media source for item {}", request.item_id))
        })
    }
}

/// Splits a comma-delimited codec parameter into ordered, non-empty tokens.
///
/// Jellyfin sends the client's supported codecs as a comma-separated,
/// preference-ordered list (e.g. `h264,hevc,vp9,av1`). The first token is the
/// preferred transcode target; the whole set is what the client can play.
fn split_codecs(param: Option<&str>) -> Vec<String> {
    param
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether NVENC hardware transcoding is enabled by the encoding options.
fn nvenc_enabled(options: &EncodingOptions) -> bool {
    options.enable_hardware_encoding
        && options.hardware_acceleration_type == HardwareAccelerationType::nvenc
}

/// The NVENC encoder for an output codec, or `None` when NVENC can't encode it.
///
/// NVENC (RTX-class GPUs) encodes H.264, HEVC and AV1; VP9 has no NVENC encoder.
fn nvenc_encoder(codec: &str) -> Option<&'static str> {
    match codec.to_ascii_lowercase().as_str() {
        "h264" => Some("h264_nvenc"),
        "hevc" | "h265" => Some("hevc_nvenc"),
        "av1" => Some("av1_nvenc"),
        _ => None,
    }
}

/// The NVENC constant-quality target (`-cq`). Lower = higher quality/bitrate;
/// 24 is visually near-transparent for 4K while keeping segments reasonable.
// TODO(config): expose as an encoding setting (Jellyfin's quality/CRF knob).
const NVENC_CQ: i32 = 24;

/// Maps an x264-style [`EncoderPreset`] to the nearest NVENC preset (`p1` fastest
/// … `p7` best quality). `auto` and unmapped presets use the balanced `p5`.
fn nvenc_preset(preset: EncoderPreset) -> &'static str {
    match preset {
        EncoderPreset::ultrafast => "p1",
        EncoderPreset::superfast => "p2",
        EncoderPreset::veryfast => "p3",
        EncoderPreset::faster | EncoderPreset::fast => "p4",
        EncoderPreset::slow => "p6",
        EncoderPreset::slower | EncoderPreset::veryslow | EncoderPreset::placebo => "p7",
        EncoderPreset::medium | EncoderPreset::auto => "p5",
    }
}

/// The NVENC video-encode tokens (quality + rate control) for `encoder`.
///
/// Constant-quality VBR (`-cq`) so 4K keeps its detail without a guessed bitrate
/// cap; a client-negotiated bitrate becomes `-maxrate`/`-bufsize` on top (the
/// cap Jellyfin applies) and a resolution bound becomes a GPU-side
/// `scale_cuda` downscale. A 10-bit HDR source targeting H.264 (browsers
/// decode only 8-bit H.264) is down-converted to `nv12` on the GPU; AV1/HEVC
/// keep the source's 10-bit HDR untouched. The subtitle-burn paths keep their
/// own in-graph conversions (a `-vf` can't coexist with `-filter_complex`).
fn nvenc_video_args(
    encoder: &str,
    options: &EncodingOptions,
    burn_sub: bool,
    output_size: Option<(i32, i32)>,
    output_bitrate: Option<i32>,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if !burn_sub {
        let mut ops: Vec<String> = Vec::new();
        if let Some((w, h)) = output_size {
            ops.push(format!("w={w}:h={h}"));
        }
        if encoder == "h264_nvenc" {
            ops.push("format=nv12".to_owned());
        }
        if !ops.is_empty() {
            a.push("-vf".to_owned());
            a.push(format!("scale_cuda={}", ops.join(":")));
        }
    }
    a.push("-preset".to_owned());
    a.push(nvenc_preset(options.encoder_preset).to_owned());
    a.push("-rc".to_owned());
    a.push("vbr".to_owned());
    a.push("-cq".to_owned());
    a.push(NVENC_CQ.to_string());
    a.push("-b:v".to_owned());
    a.push("0".to_owned());
    if let Some(bitrate) = output_bitrate {
        let bufsize = i64::from(bitrate)
            .saturating_mul(2)
            .min(i64::from(i32::MAX));
        a.push("-maxrate".to_owned());
        a.push(bitrate.to_string());
        a.push("-bufsize".to_owned());
        a.push(bufsize.to_string());
    }
    a
}

/// The transcode **target** video codec: the first client preference Ferrofin can
/// encode in realtime, else h264.
///
/// Clients send preferred codecs most-preferred-first (e.g. `av1,h264,vp9`).
/// With NVENC the hardware encodes h264/hevc/av1, so the client's top pick
/// (av1 for browsers — which keeps 10-bit HDR) is honoured. Without hardware,
/// only software libx264 (h264) is realtime-viable — software av1/vp9/hevc
/// (libaom-av1 etc.) run far below realtime and stall the player — so we fall
/// back to the broadly-compatible h264. A bare `copy` request is honoured.
fn preferred_transcode_video_codec(codecs: &[String], options: &EncodingOptions) -> String {
    let hw = nvenc_enabled(options);
    codecs
        .iter()
        .find(|c| {
            EncodingJobInfo::is_copy_codec(Some(c))
                || c.eq_ignore_ascii_case("h264")
                || (hw && nvenc_encoder(c).is_some())
        })
        .cloned()
        .unwrap_or_else(|| DEFAULT_VIDEO_CODEC.to_owned())
}

/// The client-supported codec set that drives the stream-copy decision.
///
/// A bare `copy` request instead declares the *source* codec supported (so the
/// stream copies through); otherwise the parsed preference list is the supported
/// set, falling back to the resolved `target` when the client named none.
fn supported_codecs(codecs: &[String], source: Option<&MediaStream>, target: &str) -> Vec<String> {
    if codecs
        .iter()
        .any(|c| EncodingJobInfo::is_copy_codec(Some(c)))
    {
        source
            .and_then(|s| s.codec.clone())
            .map_or_else(|| codecs.to_vec(), |c| vec![c])
    } else if codecs.is_empty() {
        vec![target.to_owned()]
    } else {
        codecs.to_vec()
    }
}

/// The default stream of `stream_type` from `streams`: the first flagged
/// `is_default`, else the first of that type.
///
/// Port of the `GetDefaultStream` selection `StreamingHelpers` applies to pick
/// the video/audio stream to transcode.
fn default_stream(streams: &[MediaStream], stream_type: MediaStreamType) -> Option<MediaStream> {
    streams
        .iter()
        .filter(|s| s.stream_type == stream_type)
        .find(|s| s.is_default)
        .or_else(|| streams.iter().find(|s| s.stream_type == stream_type))
        .cloned()
}

/// The subtitle stream at `index` from `streams`, if present.
///
/// Port of the `SubtitleStreamIndex` selection: the request names the subtitle
/// track by its media-stream index.
fn subtitle_stream(streams: &[MediaStream], index: Option<i32>) -> Option<MediaStream> {
    let index = index?;
    streams
        .iter()
        .find(|s| s.stream_type == MediaStreamType::Subtitle && s.index == index)
        .cloned()
}

/// Reads an integer query param (case-insensitive key) from a `?a=b&c=d` string.
/// The subtitle index rides in the transcode URL's query rather than a typed
/// field on the request, so the planner parses it here.
fn query_param_i32(query_string: &str, key: &str) -> Option<i32> {
    query_param(query_string, key).and_then(|v| v.parse().ok())
}

/// Reads a raw query param value (case-insensitive key) from a `?a=b&c=d` string.
fn query_param<'a>(query_string: &'a str, key: &str) -> Option<&'a str> {
    query_string
        .trim_start_matches('?')
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// Parses a `SubtitleMethod` query value (the names `StreamInfo::to_url` emits).
fn parse_subtitle_method(value: &str) -> Option<SubtitleDeliveryMethod> {
    let method = if value.eq_ignore_ascii_case("Encode") {
        SubtitleDeliveryMethod::Encode
    } else if value.eq_ignore_ascii_case("Embed") {
        SubtitleDeliveryMethod::Embed
    } else if value.eq_ignore_ascii_case("External") {
        SubtitleDeliveryMethod::External
    } else if value.eq_ignore_ascii_case("Hls") {
        SubtitleDeliveryMethod::Hls
    } else if value.eq_ignore_ascii_case("Drop") {
        SubtitleDeliveryMethod::Drop
    } else {
        return None;
    };
    Some(method)
}

/// The file extension for `segment_container` (`ts` → `ts`, `mp4` → `mp4`).
///
/// Port of `GetSegmentFileExtension`: the extension is the container name
/// (`"." + container`), so fMP4 segments are `.mp4` — matching the playlist
/// generator's `#EXT-X-MAP`/segment URIs and the served-file resolver. (An
/// earlier `.m4s` here disagreed with both, so fMP4 segments 404'd.)
fn segment_file_extension(segment_container: &str) -> &'static str {
    if segment_container.eq_ignore_ascii_case("mp4") {
        "mp4"
    } else {
        "ts"
    }
}

/// Whether `codec` is HEVC (H.265), which needs the `hvc1` codec tag + fMP4 to
/// decode in browser MSE. Port of `EncodingHelper.IsH265`.
fn is_hevc(codec: &str) -> bool {
    codec.eq_ignore_ascii_case("hevc") || codec.eq_ignore_ascii_case("h265")
}

/// The HLS `-hls_segment_type` token for `segment_container`.
///
/// Port of the `hls_segment_type` selection: `mp4` → `fmp4`, else `mpegts`.
fn hls_segment_type(segment_container: &str) -> &'static str {
    if segment_container.eq_ignore_ascii_case("mp4") {
        "fmp4"
    } else {
        "mpegts"
    }
}

#[async_trait]
impl StreamStatePlanner for FerrofinStreamStatePlanner {
    // The plan is the linear `GetStreamingState` → `GetCommandLineArguments`
    // orchestration: five clearly-labelled steps (resolve source, copy-vs-
    // transcode, populate state, build args, return). Splitting it would only
    // scatter the sequence across helpers that re-thread most of the same values.
    #[allow(clippy::too_many_lines)]
    async fn plan(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
        segment_id: Option<i32>,
    ) -> Result<TranscodePlan, ServiceError> {
        // ---- (1) RESOLVE MEDIA SOURCE (GetStreamingState) -------------------
        let media_source = self.resolve_media_source(request).await?;
        let media_path = media_source
            .path
            .clone()
            .ok_or_else(|| ServiceError::backend("media source has no path"))?;
        let run_time_ticks = media_source.run_time_ticks.unwrap_or(0);

        let video_stream = if is_audio {
            None
        } else {
            default_stream(&media_source.media_streams, MediaStreamType::Video)
        };
        let audio_stream = default_stream(&media_source.media_streams, MediaStreamType::Audio);
        // The transcode URL carries both the subtitle index and the negotiated
        // delivery (`SubtitleMethod`, written by `StreamInfo::to_url`). Honour
        // the method: only `Encode` burns the track into the video. Treating a
        // bare index as burn-in doubled subtitles — the client rendered the
        // external/embedded track the PlaybackInfo DTO promised it, on top of
        // the burn-in. An absent method with an index means Encode (the C#
        // enum default).
        let subtitle_index = query_param_i32(&request.query_string, "SubtitleStreamIndex");
        let subtitle_stream = subtitle_stream(&media_source.media_streams, subtitle_index);
        let subtitle_delivery_method = if subtitle_stream.is_some() {
            query_param(&request.query_string, "SubtitleMethod")
                .and_then(parse_subtitle_method)
                .unwrap_or(SubtitleDeliveryMethod::Encode)
        } else {
            SubtitleDeliveryMethod::Hls
        };

        let options = self.encoding_options().await;
        // `StreamState.SegmentLength`: an explicit `SegmentLength` from the
        // client wins outright, otherwise the 3s default. The request value was
        // previously gated on `encoding_thread_count >= 0` — an unrelated knob
        // that defaults to -1 (auto), so a client's negotiated `SegmentLength`
        // (device profiles emit it, `StreamInfo::to_url` forwards it) was
        // silently dropped on every default install: a 6s-segment profile got
        // 3s segments, doubling both the playlist and the number of segment
        // requests a client makes over a film.
        let segment_length_secs = request
            .segment_length
            .filter(|&secs| secs > 0)
            .unwrap_or(DEFAULT_SEGMENT_LENGTH_SECS);
        let segment_container = request
            .segment_container
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| DEFAULT_SEGMENT_CONTAINER.to_owned());

        // ---- (2) COPY-vs-TRANSCODE per stream -------------------------------
        // The client sends the video/audio codecs as a comma-delimited,
        // preference-ordered list (e.g. `h264,hevc,vp9,av1`): the whole set is
        // what it can play (drives the copy decision), and the first entry is the
        // preferred transcode target. The raw list is NOT a valid ffmpeg `-c:v`/
        // `-c:a` encoder name, so it must be resolved to a single codec here — a
        // passed-through list makes ffmpeg exit immediately and the whole
        // transcode (hence playback) fails.
        let video_codecs = split_codecs(request.video_codec.as_deref());
        let audio_codecs = split_codecs(request.audio_codec.as_deref());

        // The requested/target codecs (defaulting to the broadly-compatible
        // h264/aac). A `copy` request is honoured verbatim. The video target is
        // NOT simply the client's first preference: browsers list av1/vp9 ahead
        // of h264 for efficiency, but Ferrofin's hardware-encoder matrix is
        // deferred, so encoding to av1/vp9 in software (libaom-av1 etc.) runs far
        // slower than realtime and stalls HLS (fragLoadTimeOut). Pick the first
        // preference Ferrofin can actually encode in realtime instead.
        let requested_video_codec = preferred_transcode_video_codec(&video_codecs, &options);
        let requested_audio_codec = audio_codecs
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_AUDIO_CODEC.to_owned());

        // Build the base options from the request so the copy decision + arg
        // builder read the client's declared targets/limits. The subtitle index
        // is load-bearing: `can_stream_copy_video` must refuse `-c:v copy` when a
        // subtitle is burned in (a filter and a stream-copy can't coexist —
        // ffmpeg exits immediately and playback never starts).
        let base_request = BaseEncodingJobOptions {
            audio_codec: Some(requested_audio_codec.clone()),
            transcoding_max_audio_channels: request.transcoding_max_audio_channels,
            is_static: request.is_static,
            subtitle_stream_index: subtitle_index,
            // The PlaybackInfo-negotiated caps: they drive the bitrate params
            // (`-maxrate`/`-b:a`), the downscale filter, the framerate cap, and
            // the copy veto.
            video_bit_rate: request.video_bitrate,
            audio_bit_rate: request.audio_bitrate,
            max_width: request.max_width,
            max_height: request.max_height,
            max_framerate: request.max_framerate,
            allow_video_stream_copy: request.allow_video_stream_copy,
            allow_audio_stream_copy: request.allow_audio_stream_copy,
            ..BaseEncodingJobOptions::default()
        };

        // The declared supported codecs (the client's full set) drive the copy
        // decision; a `copy` request declares the source codec supported.
        let supported_video_codecs =
            supported_codecs(&video_codecs, video_stream.as_ref(), &requested_video_codec);
        let supported_audio_codecs =
            supported_codecs(&audio_codecs, audio_stream.as_ref(), &requested_audio_codec);

        // A probe state used only for the copy decision (the final state is
        // populated below with the resolved output codecs).
        let mut probe_state = EncodingJobInfo {
            display: self.display_names(request.item_id).await,
            base_request: base_request.clone(),
            video_stream: video_stream.clone(),
            audio_stream: audio_stream.clone(),
            subtitle_stream: subtitle_stream.clone(),
            media_source: media_source.clone(),
            output_video_codec: Some(requested_video_codec.clone()),
            output_audio_codec: Some(requested_audio_codec.clone()),
            output_video_bitrate: base_request.video_bit_rate,
            output_audio_bitrate: base_request.audio_bit_rate,
            output_audio_channels: base_request.audio_channels,
            output_container: Some(segment_container.clone()),
            output_video_sync: None,
            output_file_path: String::new(),
            input_container: media_source.container.clone(),
            is_input_video: !is_audio,
            subtitle_delivery_method,
            run_time_ticks: media_source.run_time_ticks,
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: supported_video_codecs.clone(),
            supported_audio_codecs: supported_audio_codecs.clone(),
            segment_length_secs,
            wait_for_path: None,
            segment_container: Some(segment_container.clone()),
            play_session_id: request.play_session_id.clone(),
            device_id: request.device_id.clone(),
        };

        let copy_video = video_stream
            .as_ref()
            .is_some_and(|v| self.encoding_helper.can_stream_copy_video(&probe_state, v));
        let copy_audio = audio_stream.as_ref().is_some_and(|a| {
            self.encoding_helper
                .can_stream_copy_audio(&probe_state, a, &supported_audio_codecs)
                .0
        });

        // Resolve the effective output codecs from the copy decision.
        let output_video_codec = video_stream.as_ref().map(|_| {
            if copy_video {
                "copy".to_owned()
            } else {
                requested_video_codec.clone()
            }
        });
        let output_audio_codec = audio_stream.as_ref().map(|_| {
            if copy_audio {
                "copy".to_owned()
            } else {
                requested_audio_codec.clone()
            }
        });
        let is_remuxing_video = EncodingJobInfo::is_copy_codec(output_video_codec.as_deref());

        // ---- (3) POPULATE EncodingJobInfo -----------------------------------
        // The deterministic output id (StreamingHelpers hash): a re-request with
        // the same item/source/session/device/container reuses the same job.
        let output_id = output_id(request, &segment_container, is_audio);
        let transcode_root = PathBuf::from(self.paths.transcode_path());
        let playlist_path = transcode_root.join(format!("{output_id}.m3u8"));
        let wait_for_path = segment_path(
            &playlist_path,
            segment_id.unwrap_or(0),
            segment_file_extension(&segment_container),
        );

        probe_state
            .output_video_codec
            .clone_from(&output_video_codec);
        probe_state
            .output_audio_codec
            .clone_from(&output_audio_codec);
        // Resolve the target video bitrate for a re-encode: the requested cap
        // bounded by the source bitrate and codec-efficiency scaled
        // (`GetVideoBitrateParamValue`); a copy carries no bitrate args.
        probe_state.output_video_bitrate =
            if EncodingJobInfo::is_copy_codec(output_video_codec.as_deref()) {
                None
            } else {
                let value = self.encoding_helper.video_bitrate_param_value(
                    &probe_state.base_request,
                    probe_state.video_stream.as_ref(),
                    output_video_codec.as_deref().unwrap_or(DEFAULT_VIDEO_CODEC),
                );
                (value > 0).then_some(value)
            };
        // Bitrate-driven resolution bound — port of the `ResolutionNormalizer`
        // application in `StreamingHelpers.GetStreamingState`: a bitrate-capped
        // re-encode also bounds the output resolution (an 8 Mbps ask on a 4K
        // source downscales to 1080p, like Jellyfin), unless the requested
        // bitrate already exceeds the source's.
        if let Some(output_bitrate) = probe_state.output_video_bitrate {
            let source_bitrate = probe_state.video_stream.as_ref().and_then(|v| v.bit_rate);
            let requested_not_reducing = probe_state
                .base_request
                .video_bit_rate
                .zip(source_bitrate)
                .is_some_and(|(requested, source)| requested >= source);
            let req = &mut probe_state.base_request;
            if req.max_width.is_none() && req.max_height.is_none() && requested_not_reducing {
                // Not reducing bitrate and no explicit bound: pin to the source
                // dimensions rather than downscaling.
                if let Some(video) = probe_state.video_stream.as_ref()
                    && (video.width.is_some() || video.height.is_some())
                {
                    req.max_width = video.width;
                    req.max_height = video.height;
                }
            } else {
                let output_codec = output_video_codec.as_deref().unwrap_or(DEFAULT_VIDEO_CODEC);
                let h264_equivalent =
                    ferrofin_mediaencoding::encoding_helper::helper::scale_bitrate(
                        output_bitrate,
                        output_codec,
                        "h264",
                    );
                let target_fps = req.max_framerate.or(probe_state
                    .video_stream
                    .as_ref()
                    .and_then(|v| v.average_frame_rate.or(v.real_frame_rate)));
                let resolution = ferrofin_model::dlna::ResolutionNormalizer::normalize(
                    source_bitrate,
                    output_bitrate,
                    h264_equivalent,
                    req.max_width,
                    req.max_height,
                    target_fps,
                    false,
                );
                req.max_width = resolution.max_width;
                req.max_height = resolution.max_height;
            }
        }
        // Resolve the output channel count now that the copy decision is known.
        // A focused port of `EncodingHelper.GetAudioChannels`: the requested
        // channels (which already fold in `TranscodingMaxAudioChannels`), clamped
        // to the source's channel count and, when re-encoding, to the transcoding
        // profile's hard cap. `None` → no `-ac`, so the source channels pass
        // through (unchanged from before this resolution existed).
        let audio_encoder = self.encoding_helper.audio_encoder(&probe_state);
        probe_state.output_audio_channels = resolve_output_audio_channels(
            &probe_state,
            output_audio_codec.as_deref(),
            &audio_encoder,
        );
        probe_state.output_file_path = playlist_path.to_string_lossy().into_owned();
        probe_state.wait_for_path = Some(wait_for_path);
        let state = probe_state;

        // Resolve a burned *text* subtitle to its standalone file (extracting to
        // the subtitle cache once; the C# `GetSubtitleFilePath`). The `subtitles`
        // filter otherwise re-demuxes the entire source file to load cues on
        // every ffmpeg (re)start — tens of seconds added to every play/seek.
        // On resolution failure fall back to `si=` on the original input.
        let burn_subtitle_path = if state.subtitle_delivery_method == SubtitleDeliveryMethod::Encode
            && let Some(sub) = state
                .subtitle_stream
                .as_ref()
                .filter(|s| s.is_text_subtitle_stream())
        {
            self.subtitles
                .get_subtitle_file_path(sub, &media_source)
                .await
                .ok()
        } else {
            None
        };

        // ---- (4) BUILD FFMPEG ARGS (GetCommandLineArguments) ----------------
        let arguments = self.build_arguments(
            &state,
            &media_path,
            segment_id,
            &segment_container,
            &playlist_path,
            &options,
            burn_subtitle_path.as_deref(),
        );

        // ---- (5) RETURN the TranscodePlan -----------------------------------
        Ok(TranscodePlan {
            state,
            playlist_path,
            arguments,
            media_path,
            run_time_ticks,
            segment_length_ms: segment_length_secs.saturating_mul(MS_PER_SECOND),
            is_remuxing_video,
            segment_container,
        })
    }
}

impl FerrofinStreamStatePlanner {
    /// The ffmpeg video encoder for `state`: the NVENC hardware encoder when
    /// hardware acceleration is enabled and the target codec has one, else the
    /// software [`EncodingHelper`] choice (`copy` / `libx264`).
    fn resolve_video_encoder(&self, state: &EncodingJobInfo, options: &EncodingOptions) -> String {
        let codec = state.output_video_codec.as_deref().unwrap_or("copy");
        if EncodingJobInfo::is_copy_codec(Some(codec)) {
            return "copy".to_owned();
        }
        if nvenc_enabled(options)
            && let Some(nv) = nvenc_encoder(codec)
        {
            return nv.to_owned();
        }
        self.encoding_helper.video_encoder(state)
    }

    /// Builds the full ffmpeg HLS command line (`GetCommandLineArguments`).
    ///
    /// The NEW top-level orchestrator composing the ported piecewise
    /// [`EncodingHelper`] methods: the input (`-ss` seek for a mid-stream
    /// segment, `-i`, analyzeduration/probesize), the stream mapping, the video
    /// encoder with its bitrate/quality/framerate params, the thread count, the
    /// audio encoder with its bitrate/channels, an optional subtitle burn-in, and
    /// the HLS muxer.
    // A flat, linear ffmpeg arg-builder — one push per option, in ffmpeg's
    // command order. Splitting it would only scatter the sequence across helpers
    // that re-thread the same state/paths (same rationale as `plan`).
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn build_arguments(
        &self,
        state: &EncodingJobInfo,
        media_path: &str,
        segment_id: Option<i32>,
        segment_container: &str,
        playlist_path: &std::path::Path,
        options: &EncodingOptions,
        burn_subtitle_path: Option<&str>,
    ) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();

        // Resolve the video encoder up front (it decides input hwaccel too). NVENC
        // maps the target codec to its hardware encoder; otherwise the software
        // `EncodingHelper` path (libx264 / copy) is used.
        let video_encoder = self.resolve_video_encoder(state, options);
        let copying_video = EncodingJobInfo::is_copy_codec(Some(&video_encoder));
        let nvenc_video = video_encoder.ends_with("_nvenc");
        // Burning a graphical subtitle needs the decoded frames in system memory
        // for the `overlay` filter, so we can't keep them on the GPU
        // (`-hwaccel_output_format cuda`); NVENC still uploads the filtered frames.
        let burn_graphical =
            ferrofin_mediaencoding::encoding_helper::helper::burns_graphical_subtitle(state);
        // Burning a *text* subtitle (the Encode delivery for srt/ass/…) uses the
        // `subtitles` filter, which likewise needs frames in system memory.
        let burn_text = burns_text_subtitle(state);
        let burn_sub = burn_graphical || burn_text;

        // The software re-encode's video filters (order matters): the bounded
        // downscale first (tonemap/burn-in then run on fewer pixels), then the
        // HDR→SDR tonemap chain — or a bare 8-bit down-convert for 10-bit SDR
        // sources (libx264 would otherwise emit High10, undecodable in browser
        // MSE). NVENC keeps its existing GPU-side handling (the hw filter
        // matrix is deferred).
        let sw_filters: Vec<String> = if !copying_video && !nvenc_video {
            let mut filters = Vec::new();
            let tonemap =
                ferrofin_mediaencoding::encoding_helper::helper::requires_software_tonemap(
                    state.video_stream.as_ref(),
                );
            // The fast tonemapx path tags the input frames' HDR colour metadata
            // ahead of the downscale (upstream's filter order), so untagged
            // streams still tonemap correctly.
            if tonemap && self.supports_tonemapx {
                filters.push(
                    ferrofin_mediaencoding::encoding_helper::helper::input_hdr_setparams(
                        state
                            .video_stream
                            .as_ref()
                            .and_then(|s| s.color_transfer.as_deref()),
                    )
                    .to_owned(),
                );
            }
            if let Some(scale) = ferrofin_mediaencoding::encoding_helper::helper::scale_filter(
                state.video_stream.as_ref(),
                state.base_request.max_width,
                state.base_request.max_height,
            ) {
                filters.push(scale);
            }
            if tonemap {
                // jellyfin-ffmpeg's single-pass SIMD `tonemapx` when available;
                // else the vanilla zscale/hable chain (~3× slower per frame).
                filters.push(
                    if self.supports_tonemapx {
                        ferrofin_mediaencoding::encoding_helper::helper::SOFTWARE_TONEMAPX_FILTER
                    } else {
                        ferrofin_mediaencoding::encoding_helper::helper::SOFTWARE_TONEMAP_FILTER
                    }
                    .to_owned(),
                );
            } else if ferrofin_mediaencoding::encoding_helper::helper::requires_8bit_downconvert(
                state.video_stream.as_ref(),
            ) {
                filters.push("format=yuv420p".to_owned());
            }
            filters
        } else {
            Vec::new()
        };

        // ---- input ------------------------------------------------------------
        // Seek to the segment start (GetTimeParameter): segment_id * segment_len.
        if let Some(id) = segment_id.filter(|&id| id > 0) {
            let seek_ticks =
                i64::from(id) * i64::from(state.segment_length_secs) * TICKS_PER_SECOND;
            let ss = self.encoder.get_time_parameter(seek_ticks);
            push_split(&mut args, "-ss");
            push_split(&mut args, ss.trim_start_matches("-ss").trim());
            // A copied video stream restarts at the keyframe *before* the seek
            // target, but ffmpeg's default accurate seek still trims re-encoded
            // audio forward to the exact target — the muxed audio then leads the
            // video by (target − keyframe), an audible desync on every seeked
            // copy-video stream (measured 0.7–1.6 s on real media). Start the
            // audio at the same keyframe instead; the segment already begins
            // there for video anyway.
            if copying_video {
                args.push("-noaccurate_seek".to_owned());
            }
        }
        // Decode the source on the GPU too (NVDEC) when we're NVENC-encoding, so
        // the whole pipeline stays on the card — this is what makes 4K transcode
        // run several× realtime instead of stalling on CPU HEVC decode.
        if nvenc_video {
            push_split(&mut args, "-hwaccel");
            args.push("cuda".to_owned());
            // Keep frames on the GPU for the pure transcode path; but when burning
            // a graphical subtitle, let them land in system memory so `overlay` can
            // composite the bitmap subtitle onto the video.
            if !burn_sub {
                push_split(&mut args, "-hwaccel_output_format");
                args.push("cuda".to_owned());
            }
        }
        // GetInputArgument yields the full `-i file:"…"` fragment.
        push_split(&mut args, "-analyzeduration");
        args.push("200M".to_owned());
        push_split(&mut args, "-probesize");
        args.push("1G".to_owned());
        push_split(&mut args, "-i");
        let input = self
            .encoder
            .get_input_argument(media_path, &state.media_source);
        // `get_input_argument` shell-quotes the path (`file:"…"`, inner quotes
        // `\"`-escaped) for the string-command probe path. The segment transcoder
        // spawns ffmpeg via argv (no shell), so those quotes would become part of
        // the filename and ffmpeg would fail to open it (exit 254) for any path
        // containing a space. Unquote it back into a single argv token with shlex.
        args.extend(shlex::split(&input).unwrap_or_else(|| vec![input]));

        // ---- map -------------------------------------------------------------
        push_split(&mut args, &self.encoding_helper.map_args(state));

        // ---- video -----------------------------------------------------------
        push_split(&mut args, "-c:v");
        args.push(video_encoder.clone());
        // HEVC output needs the `hvc1` codec tag to decode in browser MSE (only
        // meaningful in fMP4). Keyed on the *output* codec: the source codec when
        // copying, else the negotiated target (so `hevc_nvenc` tags too). Port of
        // `DynamicHlsController`'s `-tag:v:0 hvc1`.
        let output_codec = if copying_video {
            state.video_stream.as_ref().and_then(|s| s.codec.as_deref())
        } else {
            state.output_video_codec.as_deref()
        };
        let output_is_hevc = output_codec.is_some_and(is_hevc);
        if output_is_hevc && segment_container.eq_ignore_ascii_case("mp4") {
            push_split(&mut args, "-tag:v:0");
            args.push("hvc1".to_owned());
        }
        if !copying_video {
            if nvenc_video {
                // NVENC has its own rate-control/quality flags (no `-crf`); the
                // software `video_quality_param` would emit libx264-only args.
                let output_size = ferrofin_mediaencoding::encoding_helper::helper::output_size(
                    state.video_stream.as_ref(),
                    state.base_request.max_width,
                    state.base_request.max_height,
                );
                for tok in nvenc_video_args(
                    &video_encoder,
                    options,
                    burn_sub,
                    output_size,
                    state.output_video_bitrate,
                ) {
                    args.push(tok);
                }
            } else {
                // `video_quality_param` already includes the bitrate (`-maxrate/
                // -bufsize`) and `-r` params, mirroring `GetVideoQualityParam` —
                // no separate pushes, or ffmpeg gets them twice.
                push_split(
                    &mut args,
                    &self.encoding_helper.video_quality_param(
                        state,
                        &video_encoder,
                        options,
                        DEFAULT_ENCODER_PRESET,
                    ),
                );
            }
            let output_framerate = self.encoding_helper.framerate_param(state);
            if nvenc_video && let Some(framerate) = output_framerate {
                // NVENC skips `video_quality_param`, so it emits `-r` itself.
                push_split(&mut args, "-r");
                args.push(framerate.to_string());
            }
            // Force a keyframe at every segment boundary so the HLS muxer cuts
            // exactly on `-hls_time`. Without this, ffmpeg can only cut at the
            // encoder's natural GOP (libx264 keyint ≈250 frames ≈10 s), so the
            // first segment overruns its target length and time-to-first-segment
            // — the dominant startup latency — balloons. Port of Jellyfin's
            // `keyFrameArg`/`gopArg`.
            if nvenc_video {
                // NVENC ignores the `-force_key_frames` expression; pin the GOP
                // instead (Jellyfin's gopArg): N = ceil(segment_len × fps).
                let fps = output_framerate
                    .or_else(|| {
                        state
                            .video_stream
                            .as_ref()
                            .and_then(MediaStream::reference_frame_rate)
                    })
                    .unwrap_or(DEFAULT_KEYFRAME_FPS);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "keyint is a small positive frame count"
                )]
                let keyint = (f64::from(fps) * f64::from(state.segment_length_secs)).ceil() as i64;
                push_split(&mut args, "-g:v:0");
                args.push(keyint.to_string());
                push_split(&mut args, "-keyint_min:v:0");
                args.push(keyint.to_string());
                push_split(&mut args, "-sc_threshold:v:0");
                args.push("0".to_owned());
            } else {
                push_split(&mut args, "-force_key_frames:0");
                args.push(format!(
                    "expr:gte(t,n_forced*{})",
                    state.segment_length_secs
                ));
            }
            // The plain software filter chain (downscale/tonemap/8-bit); the
            // burn-in branches below fold `sw_filters` into their own graphs
            // instead (two `-vf`s would override each other).
            if !burn_sub && !sw_filters.is_empty() {
                args.push("-vf".to_owned());
                args.push(sw_filters.join(","));
            }
        }

        // ---- threads ---------------------------------------------------------
        let threads = self.encoding_helper.number_of_threads(Some(state), options);
        push_split(&mut args, "-threads");
        args.push(threads.to_string());

        // ---- audio -----------------------------------------------------------
        if state.audio_stream.is_some() {
            let audio_encoder = self.encoding_helper.audio_encoder(state);
            push_split(&mut args, "-c:a");
            args.push(audio_encoder.clone());
            if !EncodingJobInfo::is_copy_codec(Some(&audio_encoder)) {
                if let Some(bitrate) = self.encoding_helper.audio_bitrate_param(
                    state.output_audio_bitrate,
                    Some(&audio_encoder),
                    state.audio_stream.as_ref(),
                    state.output_audio_channels,
                ) {
                    push_split(&mut args, "-b:a");
                    args.push(bitrate.to_string());
                }
                if let Some(channels) = state.output_audio_channels {
                    push_split(&mut args, "-ac");
                    args.push(channels.to_string());
                }
                // Downmix volume boost (GetAudioFilterParam): a >stereo →
                // stereo downmix is roughly half as loud, so boost it back.
                if let Some(af) =
                    ferrofin_mediaencoding::encoding_helper::helper::audio_filter_param(
                        state, options,
                    )
                {
                    push_split(&mut args, "-af");
                    args.push(af);
                }
            }
        }

        // ---- text subtitle burn-in -------------------------------------------
        // Render the text subtitle onto the video with the `subtitles` filter
        // (libass), preferring the small extracted/external subtitle file (the
        // cached `GetSubtitleFilePath` result — the filter loads it instantly).
        // The fallback reads the track straight from the media file (`si=`
        // selects among its embedded subtitle streams), which re-demuxes the
        // whole source to find cues — correct, but slow on every start.
        if burn_text
            && !copying_video
            && let Some(sub) = state.subtitle_stream.as_ref()
        {
            use std::fmt::Write as _;
            let mut chain = String::new();
            // Downscale/tonemap ahead of the subtitle render, so the text is
            // drawn at output resolution in SDR.
            for filter in &sw_filters {
                let _ = write!(chain, "{filter},");
            }
            // An input `-ss` seek resets frame PTS to ~0, but the filter picks
            // cues by PTS — shift PTS to the absolute position for the filter,
            // then back so the muxer's `-output_ts_offset` numbering still holds.
            let offset_secs = segment_id
                .filter(|&id| id > 0)
                .map(|id| i64::from(id) * i64::from(state.segment_length_secs));
            if let Some(off) = offset_secs {
                let _ = write!(chain, "setpts=PTS+{off}/TB,");
            }
            let external_path =
                burn_subtitle_path.or_else(|| sub.path.as_deref().filter(|_| sub.is_external));
            if let Some(path) = external_path {
                let _ = write!(chain, "subtitles=f='{}'", escape_subtitle_filter_path(path));
            } else {
                let si = state
                    .media_source
                    .media_streams
                    .iter()
                    .filter(|s| s.stream_type == MediaStreamType::Subtitle && !s.is_external)
                    .position(|s| s.index == sub.index)
                    .unwrap_or(0);
                let _ = write!(
                    chain,
                    "subtitles=f='{}':si={si}",
                    escape_subtitle_filter_path(media_path)
                );
            }
            if let Some(off) = offset_secs {
                let _ = write!(chain, ",setpts=PTS-{off}/TB");
            }
            // h264 is 8-bit only; the burn path bypassed the `-vf scale_cuda`
            // down-convert, so do it inside this chain.
            if video_encoder == "h264_nvenc" {
                chain.push_str(",format=nv12");
            }
            args.push("-vf".to_owned());
            args.push(chain);
        }

        // ---- graphical subtitle burn-in --------------------------------------
        // Composite the (bitmap) subtitle stream onto the video: `overlay` takes
        // the base video and the decoded subtitle as its two inputs and emits the
        // labelled `[v]` that `map_args` maps in place of the raw video.
        if burn_graphical && let Some(sub) = state.subtitle_stream.as_ref() {
            let sub_idx = sub.index.max(0);
            let vid_idx = state.video_stream.as_ref().map_or(0, |v| v.index.max(0));
            // h264 (8-bit only) needs the overlaid frames down-converted to nv12 —
            // done inside the graph here since the separate `-vf scale_cuda` is
            // suppressed while burning (they can't coexist).
            let fmt = if video_encoder == "h264_nvenc" {
                ",format=nv12"
            } else {
                ""
            };
            push_split(&mut args, "-filter_complex");
            if sw_filters.is_empty() {
                args.push(format!("[0:{vid_idx}][0:{sub_idx}]overlay{fmt}[v]"));
            } else {
                // Downscale/tonemap the base video before compositing, so the
                // bitmap subtitle is overlaid in SDR at output resolution.
                args.push(format!(
                    "[0:{vid_idx}]{}[base];[base][0:{sub_idx}]overlay{fmt}[v]",
                    sw_filters.join(",")
                ));
            }
        }

        // ---- HLS muxer -------------------------------------------------------
        let dir = playlist_path
            .parent()
            .map_or_else(String::new, |p| p.to_string_lossy().into_owned());
        let stem = playlist_path
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let ext = segment_file_extension(segment_container);
        push_split(&mut args, "-f");
        args.push("hls".to_owned());
        push_split(&mut args, "-hls_time");
        args.push(state.segment_length_secs.to_string());
        push_split(&mut args, "-hls_playlist_type");
        args.push("vod".to_owned());
        // Write each segment to a `.tmp` and rename it into place only once fully
        // written, so a segment file appears **atomically complete**. This lets the
        // serve path wait for just segment N (not N+1) to prove completeness — it
        // roughly halves time-to-first-segment on start and on every seek, which is
        // the dominant scrub latency.
        push_split(&mut args, "-hls_flags");
        args.push("temp_file".to_owned());
        push_split(&mut args, "-hls_segment_type");
        args.push(hls_segment_type(segment_container).to_owned());
        // fMP4 (mp4) HLS writes a separate init segment carrying the codec config
        // (`#EXT-X-MAP` in the playlist). ffmpeg resolves this name relative to
        // the .m3u8 output dir, so a bare `{stem}-1.mp4` lands beside the media
        // segments where the init-segment route serves it. Port of
        // `DynamicHlsController`'s `-hls_fmp4_init_filename`.
        if segment_container.eq_ignore_ascii_case("mp4") {
            push_split(&mut args, "-hls_fmp4_init_filename");
            args.push(format!("{stem}-1.{ext}"));
        }
        // Number the output segments from the requested index (GetStartNumber).
        // A seek asks for segment N: we seek the input (`-ss`) to N and restart
        // ffmpeg, so its HLS muxer must write `stem{N}.ts` onward — without this
        // the restarted job re-numbers from 0, clobbering the original job's
        // `stem0.ts…` and never producing the segment the client is waiting for,
        // so scrubbing hangs/corrupts the stream.
        if let Some(id) = segment_id {
            push_split(&mut args, "-start_number");
            args.push(id.to_string());
        }
        // Align a seek-restarted transcode's timestamps to this segment's place
        // in the playlist. The `-ss` input seek makes ffmpeg reset output PTS to
        // ~0, so segment N would carry PTS ~0 instead of N*segment_len — the
        // player then sees a discontinuity across the seek boundary and stalls.
        // Shift the (zero-based) output timestamps forward by the seek time so
        // segment N's PTS is ≈ N*segment_len and hls.js can splice it in. (Not
        // `-copyts`, which preserves the source's arbitrary start offset and
        // desyncs the muxer's segment numbering.)
        if let Some(id) = segment_id.filter(|&id| id > 0) {
            let offset_secs = i64::from(id) * i64::from(state.segment_length_secs);
            push_split(&mut args, "-output_ts_offset");
            args.push(offset_secs.to_string());
        }
        push_split(&mut args, "-hls_segment_filename");
        args.push(format!("{dir}/{stem}%d.{ext}"));
        push_split(&mut args, "-hls_list_size");
        args.push("0".to_owned());
        args.push(playlist_path.to_string_lossy().into_owned());

        args
    }
}

/// The output audio channel count for `-ac`, a focused port of
/// `EncodingHelper.GetAudioChannels`.
///
/// Resolves the client-requested channels (`GetRequestedAudioChannels`, which
/// already prefers an explicit request over `TranscodingMaxAudioChannels`),
/// clamps to the source stream's channel count, and — when the audio is being
/// re-encoded — clamps again to `TranscodingMaxAudioChannels` as a hard ceiling.
/// Returns `None` when no cap applies, leaving the source channels untouched.
///
/// The full C# method additionally imposes the encoder's 8-channel ceiling and a
/// 3/5/7-channel HLS layout fix (adding an LFE channel); those are omitted —
/// web/TV profiles cap at 2 or 6 channels, where neither branch fires.
/// The per-encoder transcode channel ceiling. Port of Jellyfin's
/// `_audioTranscodeChannelLookup`: lossy stereo-only encoders cap at 2, the
/// surround codecs cap at 6 (5.1), and anything unlisted defaults to 8 to avoid
/// asking ffmpeg for more channels than the encoder can emit. `libfdk_aac` at 6
/// is why a 7.1 source lands at 5.1 rather than a raw `-ac 8`.
fn audio_transcode_channel_limit(encoder: &str) -> i32 {
    match encoder.to_ascii_lowercase().as_str() {
        "libmp3lame" => 2,
        "libfdk_aac" | "ac3" | "eac3" | "dca" | "mlp" | "truehd" => 6,
        _ => 8,
    }
}

fn resolve_output_audio_channels(
    state: &EncodingJobInfo,
    output_audio_codec: Option<&str>,
    audio_encoder: &str,
) -> Option<i32> {
    let codec = output_audio_codec.unwrap_or_default();
    let mut result = state.requested_audio_channels(codec);
    if let Some(input) = state
        .audio_stream
        .as_ref()
        .and_then(|s| s.channels)
        .filter(|&c| c > 0)
    {
        result = Some(result.map_or(input, |r| r.min(input)));
    }
    if !EncodingJobInfo::is_copy_codec(Some(codec)) {
        // Encoder ceiling (e.g. libfdk_aac → 6), then the client's explicit
        // `TranscodingMaxAudioChannels` cap. Port of `GetNumAudioChannelsParam`.
        let encoder_limit = audio_transcode_channel_limit(audio_encoder);
        result = Some(result.map_or(encoder_limit, |r| r.min(encoder_limit)));
        if let Some(cap) = state.base_request.transcoding_max_audio_channels
            && cap < result.unwrap_or(i32::MAX)
        {
            result = Some(cap);
        }

        // HLS only carries 1/2/6(5.1)/8(7.1)ch layouts: ffmpeg can synthesize the
        // LFE for 5→5.1 and 7→7.1; other odd layouts downmix to stereo (Apple HLS
        // authoring spec). Progressive delivery is exempt.
        if state.transcoding_type != TranscodingJobType::Progressive
            && let Some(ch) = result
            && ((ch > 2 && ch < 6) || ch == 7)
        {
            result = Some(match ch {
                5 => 6,
                7 => 8,
                _ => 2,
            });
        }
    }
    result
}

/// Whether the transcode burns a **text** subtitle (srt/ass/…) into the video
/// via the `subtitles` filter. The graphical (PGS/DVDSUB) counterpart is
/// [`burns_graphical_subtitle`](ferrofin_mediaencoding::encoding_helper::helper::burns_graphical_subtitle);
/// text subs are normally delivered externally, so this only fires when the
/// client explicitly asks for Encode (e.g. the "burn all subtitles" setting).
fn burns_text_subtitle(state: &EncodingJobInfo) -> bool {
    state.subtitle_delivery_method == SubtitleDeliveryMethod::Encode
        && state.video_stream.is_some()
        && state
            .subtitle_stream
            .as_ref()
            .is_some_and(MediaStream::is_text_subtitle_stream)
}

/// Escapes a path for use inside a `subtitles=f='…'` filter option. Port of
/// `MediaEncoder.EscapeSubtitleFilterPath`: the filtergraph parser and the
/// filter's own option parser each unescape once, so quotes are double-escaped.
fn escape_subtitle_filter_path(path: &str) -> String {
    path.replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "'\\\\\\''")
}

/// Pushes each whitespace-separated token of `fragment` as its own arg.
///
/// The [`EncodingHelper`] methods return space-joined fragments (e.g.
/// `-map 0:0 -map 0:1`); ffmpeg is spawned with an argv, so each token must be a
/// distinct element. Empty fragments contribute nothing.
fn push_split(args: &mut Vec<String>, fragment: &str) {
    args.extend(fragment.split_whitespace().map(str::to_owned));
}

/// The deterministic output id for `request` (the `StreamingHelpers` hash).
///
/// Port of the `StreamingHelpers` output-name derivation: a stable hash over the
/// fields that identify one transcode job — item, source, session, device, and
/// the output shape (container + audio/video). A re-request with the same tuple
/// reuses the same `.m3u8`/segment files.
fn output_id(request: &HlsStreamRequest, segment_container: &str, is_audio: bool) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    request.item_id.hash(&mut hasher);
    request.media_source_id.hash(&mut hasher);
    request.play_session_id.hash(&mut hasher);
    request.device_id.hash(&mut hasher);
    request.audio_codec.hash(&mut hasher);
    request.video_codec.hash(&mut hasher);
    // A burned-in subtitle changes the video, so it must key the cache — else a
    // subtitled and non-subtitled transcode of the same item would collide. The
    // method keys it too: the same index with `SubtitleMethod=Encode` vs `Embed`
    // produces different video.
    query_param_i32(&request.query_string, "SubtitleStreamIndex").hash(&mut hasher);
    query_param(&request.query_string, "SubtitleMethod").hash(&mut hasher);
    segment_container.hash(&mut hasher);
    is_audio.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The on-disk path of segment `index` for `playlist` (`GetSegmentPath`).
fn segment_path(playlist: &std::path::Path, index: i32, extension: &str) -> PathBuf {
    let folder = playlist.parent().map_or_else(PathBuf::new, PathBuf::from);
    let stem = playlist
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    folder.join(format!("{stem}{index}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::entities_media::MediaAttachment;
    use ferrofin_model::media_info::LiveStreamRequest;
    use uuid::Uuid;

    /// A fake [`MediaSourceManager`] returning a fixed source list.
    struct FakeMediaSources {
        sources: Vec<MediaSourceInfo>,
    }

    #[async_trait]
    impl MediaSourceManager for FakeMediaSources {
        async fn get_media_streams(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaStream>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_media_attachments(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaAttachment>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_playback_media_sources(
            &self,
            _item_id: Uuid,
            _user_id: Uuid,
            _allow_media_probe: bool,
            _enable_path_substitution: bool,
        ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
            Ok(self.sources.clone())
        }
        async fn get_static_media_sources(
            &self,
            _item_id: Uuid,
            _enable_path_substitution: bool,
            _user_id: Option<Uuid>,
        ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
            Ok(self.sources.clone())
        }
        async fn open_live_stream(
            &self,
            _request: &LiveStreamRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Err(ServiceError::backend("no live streams in test"))
        }
        async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
            Err(ServiceError::backend("no live streams in test"))
        }
        async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A fake [`MediaEncoder`]: only the arg-building methods are exercised.
    struct FakeEncoder;

    #[async_trait]
    impl MediaEncoder for FakeEncoder {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "ffprobe".to_owned()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            _request: &ferrofin_traits::media_encoding::MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Ok(MediaSourceInfo::default())
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            // Mirror the real encoder, which shell-quotes the path.
            format!("file:\"{input_file}\"")
        }
        fn get_time_parameter(&self, ticks: i64) -> String {
            format!("-ss {ticks}")
        }
        async fn convert_image(
            &self,
            _input_path: &str,
            _output_path: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn video_stream(codec: &str) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            index: 0,
            stream_type: MediaStreamType::Video,
            is_default: true,
            ..MediaStream::default()
        }
    }

    fn audio_stream(codec: &str) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            index: 1,
            stream_type: MediaStreamType::Audio,
            is_default: true,
            ..MediaStream::default()
        }
    }

    fn subtitle_stream(codec: &str, index: i32) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            index,
            stream_type: MediaStreamType::Subtitle,
            ..MediaStream::default()
        }
    }

    fn source(id: &str, streams: Vec<MediaStream>) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some(id.to_owned()),
            path: Some("/media/movie.mkv".to_owned()),
            container: Some("mkv".to_owned()),
            run_time_ticks: Some(90 * 60 * TICKS_PER_SECOND),
            media_streams: streams,
            ..MediaSourceInfo::default()
        }
    }

    fn planner(sources: Vec<MediaSourceInfo>) -> FerrofinStreamStatePlanner {
        planner_full(sources, false, &[])
    }

    fn planner_with_tonemapx(
        sources: Vec<MediaSourceInfo>,
        supports_tonemapx: bool,
    ) -> FerrofinStreamStatePlanner {
        planner_full(sources, supports_tonemapx, &[])
    }

    fn planner_full(
        sources: Vec<MediaSourceInfo>,
        supports_tonemapx: bool,
        encoders: &[&str],
    ) -> FerrofinStreamStatePlanner {
        let media_sources: Arc<dyn MediaSourceManager> = Arc::new(FakeMediaSources { sources });
        let encoder: Arc<dyn MediaEncoder> = Arc::new(FakeEncoder);
        let helper = EncodingHelper::with_processor_count(
            ProbedEncoders::new(encoders.iter().map(|e| (*e).to_owned()).collect()),
            8,
        );
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            "/data",
            std::path::PathBuf::from("/data/log"),
            "/config",
            "/cache",
            "/web",
        ));
        let config: Arc<dyn ServerConfigurationManager> = Arc::new(FakeConfig(Arc::clone(&paths)));
        // The disabled stub always errors, exercising the `si=` burn fallback.
        let subtitles: Arc<dyn SubtitleEncoder> =
            Arc::new(ferrofin_traits::stubs::DisabledSubtitleEncoder);
        FerrofinStreamStatePlanner::new(
            media_sources,
            encoder,
            helper,
            config,
            paths,
            subtitles,
            supports_tonemapx,
        )
    }

    /// A fake [`ServerConfigurationManager`] exposing only the application paths.
    struct FakeConfig(Arc<FerrofinServerApplicationPaths>);

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            Arc::clone(&self.0) as Arc<_>
        }
        async fn configuration(
            &self,
        ) -> Result<std::sync::Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError>
        {
            Ok(std::sync::Arc::new(
                ferrofin_model::configuration::ServerConfiguration::default(),
            ))
        }
        async fn update_configuration(
            &self,
            _configuration: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn request(source_id: &str) -> HlsStreamRequest {
        HlsStreamRequest {
            item_id: Uuid::from_u128(1),
            media_source_id: Some(source_id.to_owned()),
            play_session_id: Some("sess".to_owned()),
            device_id: Some("dev".to_owned()),
            segment_container: Some("ts".to_owned()),
            query_string: "?x=1".to_owned(),
            ..HlsStreamRequest::default()
        }
    }

    #[tokio::test]
    async fn plan_resolves_source_and_fills_runtime() {
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, None).await.unwrap();
        assert_eq!(plan.media_path, "/media/movie.mkv");
        assert_eq!(plan.run_time_ticks, 90 * 60 * TICKS_PER_SECOND);
        assert_eq!(plan.segment_length_ms, 3000);
        assert_eq!(plan.segment_container, "ts");
    }

    #[tokio::test]
    async fn plan_caps_transcoded_audio_channels_to_encoder_ceiling() {
        // A 7.1 (8ch) EAC3 the client can't take is re-encoded to AAC. libfdk_aac
        // caps at 6ch (Jellyfin's `_audioTranscodeChannelLookup`), so the downmix
        // lands at 5.1 (`-ac 6`) rather than a raw `-ac 8` passthrough.
        let mut eac3 = audio_stream("eac3");
        eac3.channels = Some(8);
        let src = source("abc", vec![video_stream("h264"), eac3]);
        let p = planner_full(vec![src], false, &["libfdk_aac"]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned()); // copy video, isolate the audio path
        let plan = p.plan(&req, false, Some(0)).await.unwrap();

        assert!(
            plan.arguments.windows(2).any(|w| w == ["-ac", "6"]),
            "7.1 must cap to the libfdk_aac ceiling of 6: {:?}",
            plan.arguments
        );
        assert!(
            !plan.arguments.windows(2).any(|w| w == ["-ac", "8"]),
            "must not pass 8 channels through to a stereo-ish endpoint: {:?}",
            plan.arguments
        );
    }

    #[test]
    fn transcode_target_skips_codecs_ferrofin_cannot_encode() {
        // Software-only: browsers list av1/vp9 first, but only libx264 (h264) is
        // realtime-viable — picking av1 would launch libaom-av1 and stall HLS.
        let sw = EncodingOptions::default();
        let av1_first = vec!["av1".to_owned(), "h264".to_owned(), "vp9".to_owned()];
        assert_eq!(preferred_transcode_video_codec(&av1_first, &sw), "h264");
        // A bare `copy` request is honoured verbatim.
        assert_eq!(
            preferred_transcode_video_codec(&["copy".to_owned()], &sw),
            "copy"
        );
        // No encodable preference (or none at all) → the h264 default.
        assert_eq!(
            preferred_transcode_video_codec(&["av1".to_owned(), "vp9".to_owned()], &sw),
            "h264"
        );
        assert_eq!(preferred_transcode_video_codec(&[], &sw), "h264");
    }

    #[test]
    fn transcode_target_prefers_client_order_with_nvenc() {
        // With NVENC the hardware encodes av1/hevc/h264, so the client's top pick
        // (av1 — keeping 10-bit HDR) is honoured instead of forcing h264.
        let nv = EncodingOptions {
            enable_hardware_encoding: true,
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
            ..EncodingOptions::default()
        };
        let av1_first = vec!["av1".to_owned(), "h264".to_owned(), "vp9".to_owned()];
        assert_eq!(preferred_transcode_video_codec(&av1_first, &nv), "av1");
        // vp9 has no NVENC encoder → skipped in favour of the next encodable.
        assert_eq!(
            preferred_transcode_video_codec(&["vp9".to_owned(), "hevc".to_owned()], &nv),
            "hevc"
        );
    }

    #[tokio::test]
    async fn plan_transcodes_hevc_source_to_libx264_not_av1() {
        // A HEVC source a browser can't decode (av1-preferred profile) must
        // transcode to libx264 (realtime), NOT `-c:v av1` (libaom-av1, far below
        // realtime -> fragLoadTimeOut).
        let src = source("abc", vec![video_stream("hevc"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("av1,h264,vp9".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("-c:v libx264"),
            "expected libx264, got: {args}"
        );
        assert!(!args.contains("-c:v av1"), "must not encode av1: {args}");
    }

    #[tokio::test]
    async fn plan_downmixes_audio_to_transcoding_max_channels() {
        // A 7.1 (8ch) source transcoded for a web profile that caps HLS audio at
        // 2ch must emit `-ac 2` — otherwise the browser's MSE pipeline can't
        // decode the AAC (the "HDR HEVC video plays, no sound" case).
        let mut audio = audio_stream("dts");
        audio.channels = Some(8);
        let src = source("abc", vec![video_stream("hevc"), audio]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        req.transcoding_max_audio_channels = Some(2);
        let plan = p.plan(&req, false, None).await.unwrap();
        let pos = plan.arguments.iter().position(|a| a == "-ac");
        assert_eq!(
            pos.map(|i| plan.arguments[i + 1].as_str()),
            Some("2"),
            "expected `-ac 2`, got: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_stereo_downmix_boosts_volume_and_prefers_libfdk() {
        // A >stereo → 2ch downmix gets `-af volume=2` (GetAudioFilterParam's
        // DownMixAudioBoost default), and `aac` maps to `libfdk_aac` when the
        // probed ffmpeg has it (GetAudioEncoder's preference order).
        let mut audio = audio_stream("dts");
        audio.channels = Some(8);
        let src = source("abc", vec![video_stream("hevc"), audio]);
        let p = planner_full(vec![src], false, &["libfdk_aac"]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        req.transcoding_max_audio_channels = Some(2);
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("-c:a libfdk_aac"),
            "libfdk_aac preferred: {args}"
        );
        assert!(args.contains("-af volume=2"), "downmix boost: {args}");

        // Same request without the 2ch cap: no downmix, no boost.
        let mut audio = audio_stream("dts");
        audio.channels = Some(8);
        let src = source("abc", vec![video_stream("hevc"), audio]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            !args.contains("-af volume"),
            "no boost without downmix: {args}"
        );
    }

    #[tokio::test]
    async fn plan_without_cap_clamps_to_source_channel_count() {
        // No cap sent → `-ac` equals the source channel count (a passthrough,
        // never an upmix): a port of `GetAudioChannels`' `?? inputChannels`
        // clamp, which also prevents a 2ch source being upmixed when a higher
        // profile cap is present.
        let mut audio = audio_stream("dts");
        audio.channels = Some(8);
        let src = source("abc", vec![video_stream("hevc"), audio]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        let pos = plan.arguments.iter().position(|a| a == "-ac");
        assert_eq!(
            pos.map(|i| plan.arguments[i + 1].as_str()),
            Some("8"),
            "expected `-ac 8` (source channels), got: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn input_path_with_spaces_is_one_unquoted_argv_token() {
        // The encoder shell-quotes the input path (`file:"…"`); the segment
        // transcoder spawns via argv, so it must reach ffmpeg as a single token
        // with the quotes stripped — otherwise ffmpeg can't open the file (exit 254)
        // and every transcode of a spaced path fails.
        let mut src = source("abc", vec![video_stream("h264"), audio_stream("ac3")]);
        src.path = Some("/media/My Movie (2010).mkv".to_owned());
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, None).await.unwrap();

        let i = plan
            .arguments
            .iter()
            .position(|a| a == "-i")
            .expect("-i present");
        assert_eq!(
            plan.arguments[i + 1],
            "file:/media/My Movie (2010).mkv",
            "input must be one token, unquoted: {:?}",
            plan.arguments
        );
        // No stray token still carries a literal quote.
        assert!(
            !plan.arguments.iter().any(|a| a.contains('"')),
            "no argv token should contain a literal quote: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_missing_source_is_not_found() {
        let src = source("abc", vec![video_stream("h264")]);
        let p = planner(vec![src]);
        let result = p.plan(&request("nope"), false, None).await;
        assert!(matches!(result, Err(ServiceError::NotFound(_))));
    }

    #[tokio::test]
    async fn plan_matches_a_guid_media_source_id_in_any_spelling() {
        // The source advertises the "N" form; clients (and Jellyfin-DB adopters
        // holding the DB's upper-case hyphenated text) may echo any spelling back.
        let src = source(
            "d37ecb9d75b0c0a8e9ecb0a864ec670e",
            vec![video_stream("h264"), audio_stream("aac")],
        );
        let p = planner(vec![src]);
        for id in [
            "d37ecb9d75b0c0a8e9ecb0a864ec670e",
            "D37ECB9D-75B0-C0A8-E9EC-B0A864EC670E",
            "d37ecb9d-75b0-c0a8-e9ec-b0a864ec670e",
        ] {
            let plan = p.plan(&request(id), false, None).await.unwrap();
            assert_eq!(plan.media_path, "/media/movie.mkv", "{id}");
        }
    }

    #[tokio::test]
    async fn plan_falls_back_to_the_first_source_when_the_id_is_the_item_id() {
        // `StreamingHelpers`: a MediaSourceId equal to the item id selects the first source.
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let plan = p
            .plan(
                &request(&Uuid::from_u128(1).simple().to_string()),
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(plan.media_path, "/media/movie.mkv");
    }

    #[tokio::test]
    async fn plan_burns_embedded_text_subtitle_and_refuses_video_copy() {
        // An h264 source the client can play would normally stream-copy; a
        // selected text subtitle (SubtitleStreamIndex in the transcode URL)
        // must force a re-encode with a `subtitles` burn filter — `-c:v copy`
        // plus a filter makes ffmpeg exit immediately.
        let src = source(
            "abc",
            vec![
                video_stream("h264"),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        );
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.query_string = "?SubtitleStreamIndex=2".to_owned();
        let plan = p.plan(&req, false, Some(0)).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("-vf subtitles=f='/media/movie.mkv':si=0"),
            "expected embedded si= burn filter, got: {args}"
        );
        assert!(!args.contains("-c:v copy"), "must not stream-copy: {args}");
    }

    #[tokio::test]
    async fn plan_seek_wraps_text_burn_in_setpts_shift() {
        // A seek restart input-seeks with `-ss`, resetting frame PTS to ~0; the
        // subtitles filter picks cues by PTS, so the chain must shift PTS to
        // the absolute position for the filter and back for the muxer.
        let src = source(
            "abc",
            vec![
                video_stream("hevc"),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        );
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.query_string = "?SubtitleStreamIndex=2".to_owned();
        let plan = p.plan(&req, false, Some(3)).await.unwrap();
        let vf = plan
            .arguments
            .iter()
            .position(|a| a == "-vf")
            .map(|i| plan.arguments[i + 1].as_str())
            .expect("-vf present");
        // 3 segments * 3 s = 9 s shift.
        assert!(
            vf.starts_with("setpts=PTS+9/TB,subtitles=") && vf.contains(",setpts=PTS-9/TB"),
            "expected setpts sandwich, got: {vf}"
        );
    }

    #[tokio::test]
    async fn plan_without_subtitle_index_has_no_burn_filter() {
        let src = source(
            "abc",
            vec![
                video_stream("hevc"),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        );
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, Some(0)).await.unwrap();
        assert!(
            !plan.arguments.iter().any(|a| a.contains("subtitles=")),
            "no subtitle selected → no burn filter: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_honors_non_encode_subtitle_method_no_burn() {
        // A `SubtitleMethod` other than Encode on the URL means the client
        // renders the track itself (external VTT / embedded); burning it in
        // anyway put the same subtitle on screen twice.
        for method in ["Embed", "External", "Hls", "Drop"] {
            let src = source(
                "abc",
                vec![
                    video_stream("hevc"),
                    audio_stream("aac"),
                    subtitle_stream("subrip", 2),
                ],
            );
            let p = planner(vec![src]);
            let mut req = request("abc");
            req.query_string = format!("?SubtitleStreamIndex=2&SubtitleMethod={method}");
            let plan = p.plan(&req, false, Some(0)).await.unwrap();
            assert!(
                !plan.arguments.iter().any(|a| a.contains("subtitles=")),
                "SubtitleMethod={method} must not burn: {:?}",
                plan.arguments
            );
        }
    }

    #[tokio::test]
    async fn plan_explicit_subtitle_method_encode_burns() {
        let src = source(
            "abc",
            vec![
                video_stream("hevc"),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        );
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.query_string = "?SubtitleStreamIndex=2&SubtitleMethod=Encode".to_owned();
        let plan = p.plan(&req, false, Some(0)).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("subtitles=f='/media/movie.mkv':si=0"),
            "explicit Encode must burn: {args}"
        );
    }

    #[test]
    fn output_id_keys_on_subtitle_method() {
        let mut req = request("abc");
        req.query_string = "?SubtitleStreamIndex=2&SubtitleMethod=Encode".to_owned();
        let encode = output_id(&req, "ts", false);
        req.query_string = "?SubtitleStreamIndex=2&SubtitleMethod=Embed".to_owned();
        let embed = output_id(&req, "ts", false);
        assert_ne!(encode, embed, "burn vs no-burn must not share cache files");
    }

    #[test]
    fn escape_subtitle_filter_path_escapes_metacharacters() {
        // Port of `MediaEncoder.EscapeSubtitleFilterPath` oracle values.
        assert_eq!(escape_subtitle_filter_path("/a/b.mkv"), "/a/b.mkv");
        assert_eq!(escape_subtitle_filter_path("C:\\a.mkv"), "C\\:/a.mkv");
        assert_eq!(escape_subtitle_filter_path("/a's.mkv"), "/a'\\\\\\''s.mkv");
    }

    #[tokio::test]
    async fn plan_fmp4_hevc_gets_hvc1_tag_and_init_segment() {
        // A hevc source delivered as fMP4 (mp4) must carry `-tag:v:0 hvc1` (so
        // browser MSE can decode HEVC) and a `-hls_fmp4_init_filename` (the
        // `#EXT-X-MAP` init segment). Its segments are `.mp4`, matching the
        // playlist — an earlier `.m4s` here made every fMP4 segment 404.
        let src = source("abc", vec![video_stream("hevc"), audio_stream("eac3")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.segment_container = Some("mp4".to_owned());
        req.video_codec = Some("hevc".to_owned()); // client supports hevc → copy
        let plan = p.plan(&req, false, Some(0)).await.unwrap();

        assert!(
            plan.arguments.windows(2).any(|w| w == ["-c:v", "copy"]),
            "{:?}",
            plan.arguments
        );
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-tag:v:0", "hvc1"]),
            "hevc fMP4 needs the hvc1 tag: {:?}",
            plan.arguments
        );
        assert!(
            plan.arguments
                .windows(2)
                .any(|w| w == ["-hls_segment_type", "fmp4"]),
            "{:?}",
            plan.arguments
        );
        assert!(
            plan.arguments
                .iter()
                .any(|a| a == "-hls_fmp4_init_filename"),
            "fMP4 needs an init segment: {:?}",
            plan.arguments
        );
        assert!(
            plan.arguments.iter().any(|a| a.ends_with("%d.mp4")),
            "fMP4 segments are .mp4: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_ts_hevc_gets_no_hvc1_tag() {
        // The `hvc1` tag is an fMP4 concept; the TS path must not emit it (it
        // would break the mpegts mux) — h264/SDR content stays on TS unchanged.
        let src = source("abc", vec![video_stream("hevc"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc"); // segment_container defaults to "ts"
        req.video_codec = Some("hevc".to_owned());
        let plan = p.plan(&req, false, Some(0)).await.unwrap();
        assert!(
            !plan.arguments.iter().any(|a| a == "-tag:v:0"),
            "TS must not carry the hvc1 tag: {:?}",
            plan.arguments
        );
        assert!(
            !plan
                .arguments
                .iter()
                .any(|a| a == "-hls_fmp4_init_filename")
        );
    }

    #[tokio::test]
    async fn plan_transcodes_h264_to_libx264_when_no_supported_codecs() {
        // Default request: video codec defaults to h264, supported list is the
        // requested target, so an h264 source can copy... unless we force a
        // transcode by requesting a codec the source doesn't have.
        let src = source("abc", vec![video_stream("mpeg4"), audio_stream("mp3")]);
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, Some(0)).await.unwrap();
        // Source is mpeg4, target defaults to h264 → not copyable → libx264.
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-c:v", "libx264"]),
            "{:?}",
            plan.arguments
        );
        assert!(!plan.is_remuxing_video);
        // Audio target defaults to aac, source is mp3 → transcode to aac.
        assert!(plan.arguments.windows(2).any(|w| w == ["-c:a", "aac"]));
    }

    #[tokio::test]
    async fn plan_resolves_codec_preference_list_to_single_codec() {
        // The client sends comma-delimited preference lists. The source is hevc
        // (in the video list → copy) with eac3 audio (NOT in the audio list →
        // transcode to the first listed codec, aac). The raw list must never
        // reach ffmpeg's `-c:v`/`-c:a` (an invalid encoder name fails the job).
        let src = source("abc", vec![video_stream("hevc"), audio_stream("eac3")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264,hevc,vp9,av1".to_owned());
        req.audio_codec = Some("aac,mp3,mp2,opus,flac,vorbis".to_owned());
        let plan = p.plan(&req, false, Some(0)).await.unwrap();

        // hevc is supported → copy video; eac3 unsupported → transcode to aac.
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-c:v", "copy"]),
            "hevc source the client supports must copy: {:?}",
            plan.arguments
        );
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-c:a", "aac"]),
            "eac3 the client can't play must transcode to aac: {:?}",
            plan.arguments
        );
        // No argv token is a comma-joined codec list.
        assert!(
            !plan.arguments.iter().any(|a| a.contains(',')),
            "no codec-list token may reach ffmpeg: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_copies_matching_codec() {
        // Request video_codec=copy → declares the source codec supported → copy.
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("copy".to_owned());
        req.audio_codec = Some("copy".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        assert!(plan.is_remuxing_video);
        assert!(plan.arguments.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert!(plan.arguments.windows(2).any(|w| w == ["-c:a", "copy"]));
    }

    #[tokio::test]
    async fn plan_honors_the_requested_segment_length() {
        // `StreamState.SegmentLength`: an explicit request value wins. This was
        // gated on `encoding_thread_count >= 0`, which defaults to -1 — so the
        // negotiated length was dropped on every default install and a 6s
        // profile silently got 3s segments (double the playlist, double the
        // segment requests). `plan()` here reads the default EncodingOptions.
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.segment_length = Some(6);
        let plan = p.plan(&req, false, Some(0)).await.unwrap();
        assert_eq!(plan.segment_length_ms, 6000);
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-hls_time", "6"]),
            "{:?}",
            plan.arguments
        );

        // Absent → the 3s default.
        let plan = p.plan(&request("abc"), false, Some(0)).await.unwrap();
        assert_eq!(plan.segment_length_ms, 3000);

        // A degenerate 0 would make the playlist generator divide by zero
        // (`HlsError::InvalidOperation` → 500); it falls back to the default
        // instead. Deliberate divergence: upstream takes the 0 verbatim.
        let mut req = request("abc");
        req.segment_length = Some(0);
        let plan = p.plan(&req, false, Some(0)).await.unwrap();
        assert_eq!(plan.segment_length_ms, 3000);
    }

    #[tokio::test]
    async fn plan_builds_hls_muxer_args() {
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, Some(0)).await.unwrap();
        assert!(plan.arguments.windows(2).any(|w| w == ["-f", "hls"]));
        assert!(plan.arguments.windows(2).any(|w| w == ["-hls_time", "3"]));
        assert!(
            plan.arguments
                .windows(2)
                .any(|w| w == ["-hls_playlist_type", "vod"])
        );
        assert!(
            plan.arguments
                .windows(2)
                .any(|w| w == ["-hls_segment_type", "mpegts"])
        );
        // The playlist path is the last argument, an .m3u8 under the transcode dir.
        let last = plan.arguments.last().unwrap();
        assert_eq!(
            std::path::Path::new(last)
                .extension()
                .and_then(|e| e.to_str()),
            Some("m3u8"),
            "{last}"
        );
        assert!(last.contains("transcodes"), "{last}");
        assert_eq!(plan.playlist_path.to_string_lossy(), last.as_str());
    }

    #[tokio::test]
    async fn plan_seeks_for_nonzero_segment() {
        let src = source("abc", vec![video_stream("mpeg4"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), true, Some(2)).await.unwrap();
        // Audio-only plan: segment 2 seeks to 2 * 3s = 6s worth of ticks.
        assert!(
            plan.arguments.iter().any(|a| a == "-ss"),
            "{:?}",
            plan.arguments
        );
        // ...and the HLS muxer must number segments from 2 so it writes stem2.ts
        // (a seek-restart re-numbering from 0 would clobber the original job and
        // never produce the requested segment — scrubbing would hang).
        assert!(
            plan.arguments
                .windows(2)
                .any(|w| w == ["-start_number", "2"]),
            "seek segment must set -start_number: {:?}",
            plan.arguments
        );
        // ...and shift output timestamps to the segment's playlist time (2 * 3s =
        // 12s) so the player can splice the seek-restarted segment (else PTS ~0
        // → discontinuity → stall).
        assert!(
            plan.arguments
                .windows(2)
                .any(|w| w == ["-output_ts_offset", "6"]),
            "seek segment must set -output_ts_offset to N*segment_len: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_seek_with_video_copy_disables_accurate_seek() {
        // A seeked copy-video stream: ffmpeg's accurate seek would trim the
        // re-encoded audio forward to the seek target while the copied video
        // restarts at the previous keyframe — audio then leads video by
        // (target − keyframe) in every segment after a seek. `-noaccurate_seek`
        // starts both streams at the keyframe.
        let src = source("abc", vec![video_stream("hevc"), audio_stream("eac3")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.segment_container = Some("mp4".to_owned());
        req.video_codec = Some("hevc".to_owned()); // client supports hevc → copy
        let plan = p.plan(&req, false, Some(2)).await.unwrap();
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-c:v", "copy"]),
            "{:?}",
            plan.arguments
        );
        assert!(
            plan.arguments.iter().any(|a| a == "-noaccurate_seek"),
            "seek + video copy must disable accurate seek: {:?}",
            plan.arguments
        );

        // A seeked re-encode discards up to the target on BOTH streams — accurate
        // seek is correct there, and the flag must not appear.
        let src = source("abc", vec![video_stream("hevc"), audio_stream("eac3")]);
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, Some(2)).await.unwrap();
        assert!(
            !plan.arguments.iter().any(|a| a == "-noaccurate_seek"),
            "re-encode seek keeps accurate seek: {:?}",
            plan.arguments
        );

        // No seek (segment 0): no flag either way.
        let src = source("abc", vec![video_stream("hevc"), audio_stream("eac3")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.segment_container = Some("mp4".to_owned());
        req.video_codec = Some("hevc".to_owned());
        let plan = p.plan(&req, false, Some(0)).await.unwrap();
        assert!(
            !plan.arguments.iter().any(|a| a == "-noaccurate_seek"),
            "no seek, no flag: {:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_omits_start_number_for_the_playlist_build() {
        // The variant-playlist build passes segment_id=None; there is no seek, so
        // no -start_number (segments number from 0 as usual).
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let plan = p.plan(&request("abc"), false, None).await.unwrap();
        assert!(
            !plan.arguments.iter().any(|a| a == "-start_number"),
            "{:?}",
            plan.arguments
        );
        // Segment 0 (initial play) also needs no timestamp shift — it starts at 0.
        assert!(
            !plan.arguments.iter().any(|a| a == "-output_ts_offset"),
            "{:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn output_id_is_deterministic_and_shape_sensitive() {
        let req = request("abc");
        let a = output_id(&req, "ts", false);
        let b = output_id(&req, "ts", false);
        let c = output_id(&req, "mp4", false);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn segment_helpers_map_containers() {
        assert_eq!(segment_file_extension("ts"), "ts");
        assert_eq!(segment_file_extension("mp4"), "mp4");
        assert_eq!(hls_segment_type("ts"), "mpegts");
        assert_eq!(hls_segment_type("mp4"), "fmp4");
    }

    /// A 4K HDR10 HEVC video stream (the heavy-transcode benchmark shape).
    fn hdr_4k_video_stream(codec: &str) -> MediaStream {
        MediaStream {
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            bit_rate: Some(60_000_000),
            color_transfer: Some("smpte2084".to_owned()),
            ..video_stream(codec)
        }
    }

    #[tokio::test]
    async fn plan_with_caps_downscales_tonemaps_and_caps_bitrate() {
        // The negotiated 8 Mbps / 1920-wide re-encode of a 4K HDR source must
        // scale to 1080p, tonemap to SDR, and cap the encoder bitrate — the
        // parity gaps the 2026-07-30 benchmark surfaced (Jellyfin: 3.7 s TTFS;
        // Ferrofin encoding full 4K HDR: 47 s).
        let src = source(
            "abc",
            vec![hdr_4k_video_stream("hevc"), audio_stream("aac")],
        );
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.video_bitrate = Some(8_000_000);
        req.max_width = Some(1920);
        req.max_height = Some(1080);
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-c:v libx264"), "re-encode expected: {args}");
        // Force a keyframe on every 3 s segment boundary so the HLS muxer cuts on
        // `-hls_time` instead of libx264's ~10 s natural GOP — halving TTFS.
        assert!(
            args.contains("-force_key_frames:0 expr:gte(t,n_forced*3)"),
            "segment-boundary keyframes expected: {args}"
        );
        assert!(
            args.contains("scale=1920:1080"),
            "bounded downscale expected: {args}"
        );
        assert!(
            args.contains("tonemap=tonemap=hable") && args.contains("format=yuv420p"),
            "HDR→SDR tonemap chain expected: {args}"
        );
        assert!(
            args.contains("-maxrate 8000000") && args.contains("-bufsize 16000000"),
            "bitrate cap expected: {args}"
        );
        // Exactly once: `video_quality_param` already carries the bitrate pair,
        // and a second `video_bitrate_param` push used to duplicate it.
        assert_eq!(
            args.matches("-maxrate").count(),
            1,
            "bitrate cap emitted exactly once: {args}"
        );
    }

    #[tokio::test]
    async fn plan_with_tonemapx_support_uses_single_pass_tonemap() {
        // When the discovered ffmpeg reports `tonemapx` (jellyfin-ffmpeg), the
        // HDR re-encode must use it — `setparams` (input HDR tag) → downscale →
        // `tonemapx`, upstream's filter order — instead of the zscale chain.
        let src = source(
            "abc",
            vec![hdr_4k_video_stream("hevc"), audio_stream("aac")],
        );
        let p = planner_with_tonemapx(vec![src], true);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.video_bitrate = Some(8_000_000);
        req.max_width = Some(1920);
        req.max_height = Some(1080);
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        let vf = plan
            .arguments
            .iter()
            .position(|a| a == "-vf")
            .map(|i| plan.arguments[i + 1].as_str())
            .expect("-vf expected");
        assert_eq!(
            vf,
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,\
             scale=1920:1080,\
             tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p",
        );
        assert!(!args.contains("zscale"), "no zscale fallback: {args}");
    }

    #[tokio::test]
    async fn plan_without_caps_keeps_source_resolution_but_still_tonemaps() {
        // No negotiated caps: no scale filter and no -maxrate, but an HDR
        // source re-encoded to (SDR) h264 still needs the tonemap chain.
        let src = source(
            "abc",
            vec![hdr_4k_video_stream("hevc"), audio_stream("aac")],
        );
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        // (`zscale=` from the tonemap chain is expected; a downscale would
        // prefix the chain as `-vf scale=…`.)
        assert!(
            !args.contains("-vf scale="),
            "no downscale without caps: {args}"
        );
        assert!(!args.contains("-maxrate"), "no bitrate cap: {args}");
        assert!(
            args.contains("tonemap=tonemap=hable"),
            "tonemap still applies: {args}"
        );
    }

    #[tokio::test]
    async fn plan_sdr_10bit_source_gets_8bit_downconvert_only() {
        // A 10-bit SDR source re-encoded to h264 needs `format=yuv420p` (else
        // libx264 emits High10, undecodable in browser MSE) but no tonemap.
        let mut stream = video_stream("hevc");
        stream.bit_depth = Some(10);
        stream.width = Some(1920);
        stream.height = Some(1080);
        let src = source("abc", vec![stream, audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("-vf format=yuv420p"),
            "8-bit down-convert: {args}"
        );
        assert!(!args.contains("tonemap"), "SDR source: no tonemap: {args}");
    }

    #[tokio::test]
    async fn plan_honors_allow_video_stream_copy_false() {
        // A copy-eligible source (client supports hevc) must re-encode when the
        // request forbade video stream copy (PlaybackInfo appends
        // `allowVideoStreamCopy=false`).
        let src = source("abc", vec![video_stream("hevc"), audio_stream("aac")]);
        let p = planner(vec![src.clone()]);
        let mut req = request("abc");
        req.video_codec = Some("hevc,h264".to_owned());
        let plan = p.plan(&req, false, None).await.unwrap();
        assert!(
            plan.arguments.join(" ").contains("-c:v copy"),
            "control: copy expected when allowed"
        );

        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("hevc,h264".to_owned());
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            !args.contains("-c:v copy"),
            "copy forbidden by the request: {args}"
        );
    }

    #[tokio::test]
    async fn plan_bitrate_only_request_still_downscales() {
        // Jellyfin derives the resolution bound from the bitrate alone
        // (ResolutionNormalizer): an 8 Mbps ask on a 4K source must downscale
        // to 1920-wide even when the URL carries no MaxWidth.
        let src = source(
            "abc",
            vec![hdr_4k_video_stream("hevc"), audio_stream("aac")],
        );
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.video_bitrate = Some(8_000_000);
        let plan = p.plan(&req, false, None).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("scale=1920:1080"),
            "bitrate-driven downscale expected: {args}"
        );
        assert!(args.contains("-maxrate 8000000"), "cap expected: {args}");
    }

    #[test]
    fn nvenc_args_carry_negotiated_cap_and_gpu_downscale() {
        let options = EncodingOptions::default();
        // A negotiated 8 Mbps / 1080p bound: GPU downscale + rate cap on top of
        // the CQ-VBR quality target.
        let args = nvenc_video_args(
            "av1_nvenc",
            &options,
            false,
            Some((1920, 1080)),
            Some(7_616_000),
        )
        .join(" ");
        assert!(args.contains("-vf scale_cuda=w=1920:h=1080"), "{args}");
        assert!(
            args.contains("-maxrate 7616000 -bufsize 15232000"),
            "{args}"
        );
        // h264 adds the 8-bit convert inside the same scale_cuda.
        let args =
            nvenc_video_args("h264_nvenc", &options, false, Some((1280, 720)), None).join(" ");
        assert!(
            args.contains("scale_cuda=w=1280:h=720:format=nv12"),
            "{args}"
        );
        assert!(!args.contains("-maxrate"), "{args}");
        // No caps: h264 keeps its bare format convert, others no -vf at all.
        let args = nvenc_video_args("hevc_nvenc", &options, false, None, None).join(" ");
        assert!(!args.contains("-vf"), "{args}");
        // Burning suppresses the -vf (the burn graph owns the conversion).
        let args =
            nvenc_video_args("h264_nvenc", &options, true, Some((1920, 1080)), None).join(" ");
        assert!(!args.contains("-vf"), "{args}");
    }
}
