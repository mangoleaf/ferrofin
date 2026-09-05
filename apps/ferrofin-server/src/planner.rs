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
//! (only stored/embedded burn-in). The hardware matrix is the work of
//! the hardware-transcoding roadmap; the subtitle-provider fan-out is work item 5
//! in that plan's list.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_core::FerrofinServerApplicationPaths;
use ferrofin_hls::{PlaylistKind, StreamStatePlanner, TranscodePlan};
use ferrofin_mediaencoding::encoding_helper::hw;
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingHelper, EncodingJobInfo, FfmpegCapabilities,
};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{
    EncoderPreset, HardwareAccelerationType, MediaStreamType, VideoType,
};
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
/// when the configured preset is `auto`, for a VOD job. Port of
/// `DynamicHlsController.DefaultVodEncoderPreset = EncoderPreset.veryfast`.
const DEFAULT_ENCODER_PRESET: EncoderPreset = EncoderPreset::veryfast;

/// The default encoder preset for an EVENT (`live.m3u8`) job, which must keep
/// up with real time. Port of
/// `DynamicHlsController.DefaultEventEncoderPreset = EncoderPreset.superfast`.
const DEFAULT_EVENT_ENCODER_PRESET: EncoderPreset = EncoderPreset::superfast;

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
    encoding_helper: EncodingHelper<FfmpegCapabilities>,
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
    /// Resolves the VAAPI device's driver and Vulkan interop on first use.
    ///
    /// Built here rather than injected because everything it needs — the
    /// ffmpeg path and the boot capabilities — is already held, and it is
    /// pure cache in front of a probe.
    vaapi_prober: crate::vaapi_probe::VaapiProber,
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
    ///   and the stream-copy decision (its [`FfmpegCapabilities`] carrying the
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
        encoding_helper: EncodingHelper<FfmpegCapabilities>,
        config: Arc<dyn ServerConfigurationManager>,
        paths: Arc<FerrofinServerApplicationPaths>,
        subtitles: Arc<dyn SubtitleEncoder>,
        supports_tonemapx: bool,
    ) -> Self {
        Self {
            vaapi_prober: crate::vaapi_probe::VaapiProber::new(
                std::path::PathBuf::from(encoder.encoder_path()),
                encoding_helper.capabilities().clone(),
            ),
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
    /// an open live stream (`live_stream_id`) wins outright and is returned as
    /// its own source; otherwise fetch the item's static media sources and
    /// select the one matching the request's `media_source_id` (defaulting to
    /// the first when unspecified).
    async fn resolve_media_source(
        &self,
        request: &HlsStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        // `StreamingHelpers.GetStreamingState`: an open live stream wins over
        // everything — its source is the buffered copy of the tuner, and going
        // back to the static sources here would dial the tuner a second time.
        if let Some(live_stream_id) = request
            .live_stream_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return self.media_sources.get_live_stream(live_stream_id).await;
        }

        let sources = self
            .media_sources
            .get_static_media_sources(request.item_id, false, None)
            .await?;

        // Match the requested id, else when the "media source id" is really the
        // item id, the first source.
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

/// Whether the configured accelerator is one Ferrofin supports.
///
/// Naming a vendor encoder is not enough on its own — the filter chain and the
/// encoder-parameter arms have to exist too, or the job gets an encoder with no
/// preset, no rate control and no scaler, which is worse than the software
/// transcode it would otherwise have had.
///
/// **NVENC, VAAPI and QSV are supported. AMF, VideoToolbox, RKMPP and V4L2M2M
/// are not**, and that is a project decision rather than a gap waiting to be
/// filled: their chains cannot be verified without the hardware to run them on,
/// and shipping an unverified hardware pipeline is how you get silent green
/// frames. Owner's call, 2026-08-26. Un-supporting path: port the vendor's
/// chain and its `GetEncoderParam`/`GetVideoBitrateParam` arms, verify on real
/// hardware, then add it here.
///
/// Selecting an unsupported accelerator is safe — the job takes the software
/// path — but it is not silent; see [`warn_unsupported_accelerator`].
fn hardware_path_is_ported(options: &EncodingOptions) -> bool {
    matches!(
        options.hardware_acceleration_type,
        HardwareAccelerationType::nvenc
            | HardwareAccelerationType::vaapi
            | HardwareAccelerationType::qsv
    )
}

/// Says once, loudly, that a configured accelerator will not be used.
///
/// Without this the operator picks AMF in the dashboard, gets a working but
/// software transcode, and has nothing to tell them why their GPU is idle —
/// which is indistinguishable from hardware acceleration being broken.
fn warn_unsupported_accelerator(options: &EncodingOptions) {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    if options.hardware_acceleration_type == HardwareAccelerationType::none
        || hardware_path_is_ported(options)
    {
        return;
    }
    if WARNED.set(()).is_ok() {
        tracing::warn!(
            accelerator = ?options.hardware_acceleration_type,
            "this hardware accelerator is not supported by Ferrofin; transcoding \
             in software instead. Supported: nvenc, vaapi, qsv."
        );
    }
}

/// Whether hardware transcoding is enabled and the running ffmpeg has an
/// encoder for `codec` on the configured accelerator.
///
/// Asks the ported encoder dispatch rather than a hand-kept table: a codec is
/// hardware-encodable exactly when that dispatch returns something other than
/// the software encoder it would otherwise pick.
fn hardware_encodes(
    codec: &str,
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    video_type: Option<VideoType>,
) -> bool {
    if !options.enable_hardware_encoding || !hardware_path_is_ported(options) {
        return false;
    }
    // The video type is load-bearing, not decoration: hardware encoding is
    // refused for some disc/folder rips, and answering "yes" for one of those
    // lets a codec be chosen on the strength of a hardware encoder and then
    // land on software libsvtav1/libx265 — the sub-realtime stall this
    // function exists to avoid.
    let hw = hw::encoder::video_encoder(
        Some(codec),
        caps,
        options.hardware_acceleration_type,
        video_type,
        true,
    );
    let sw = hw::encoder::video_encoder(
        Some(codec),
        caps,
        HardwareAccelerationType::none,
        video_type,
        false,
    );
    hw != sw
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
fn preferred_transcode_video_codec(
    codecs: &[String],
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    video_type: Option<VideoType>,
) -> String {
    codecs
        .iter()
        .find(|c| {
            EncodingJobInfo::is_copy_codec(Some(c))
                || c.eq_ignore_ascii_case("h264")
                || hardware_encodes(c, caps, options, video_type)
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

/// Port of `StreamingHelpers.ParseStreamOptions`: every raw query pair whose
/// key starts with a lower-case letter is a per-codec stream option (the
/// generated transcode URL lower-cases e.g. `h264-profile`, `aac-profile`,
/// `hevc-level`), keyed and valued as decoded. A repeated key keeps its last
/// value (the dictionary indexer).
fn parse_stream_options(query_string: &str) -> Vec<(String, String)> {
    let mut options: Vec<(String, String)> = Vec::new();
    for (key, value) in ferrofin_hls::query_pairs(query_string) {
        if !key.chars().next().is_some_and(char::is_lowercase) {
            continue;
        }
        if let Some((_, existing)) = options.iter_mut().find(|(k, _)| *k == key) {
            *existing = value;
        } else {
            options.push((key, value));
        }
    }
    options
}

/// Port of `EncodingHelper.LosslessAudioCodecs.Contains(codec)`: the output
/// codecs whose HLS bitrate is the source's own (`alac`, `ape`, `flac`, `mlp`,
/// `truehd`, `wavpack`).
fn is_lossless_audio_codec(codec: &str) -> bool {
    ["alac", "ape", "flac", "mlp", "truehd", "wavpack"]
        .iter()
        .any(|c| codec.eq_ignore_ascii_case(c))
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
        kind: PlaylistKind,
    ) -> Result<TranscodePlan, ServiceError> {
        // ---- (1) RESOLVE MEDIA SOURCE (GetStreamingState) -------------------
        let media_source = self.resolve_media_source(request).await?;
        // `StreamingHelpers.GetStreamingState`, live branch: "cap the max
        // bitrate when it is too high. This is usually due to ffmpeg is unable
        // to probe the source liveTV streams' bitrate." A client asking for its
        // "auto" ceiling (100-140 Mbps) would otherwise put that straight into
        // `-maxrate` and suppress the downscale filter for a channel the tuner
        // itself caps far lower.
        let requested_video_bitrate = match (
            request.video_bitrate,
            media_source.fallback_max_streaming_bitrate,
        ) {
            (Some(requested), Some(fallback)) => Some(requested.min(fallback)),
            (requested, _) => requested,
        };
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
        let subtitle_index = request
            .subtitle_stream_index
            .or_else(|| query_param_i32(&request.query_string, "SubtitleStreamIndex"));
        let subtitle_stream = subtitle_stream(&media_source.media_streams, subtitle_index);
        let requested_subtitle_method = request
            .subtitle_method
            .as_deref()
            .or_else(|| query_param(&request.query_string, "SubtitleMethod"))
            .and_then(parse_subtitle_method);
        let subtitle_delivery_method = if subtitle_stream.is_some() {
            requested_subtitle_method.unwrap_or(SubtitleDeliveryMethod::Encode)
        } else {
            // No selected stream: the request DTO's `SubtitleMethod` default
            // (`subtitleMethod ?? SubtitleDeliveryMethod.External` on every HLS
            // route). The master playlist keys its subtitle group off this, so
            // it must not read as `Hls` when nothing was selected.
            requested_subtitle_method.unwrap_or(SubtitleDeliveryMethod::External)
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
        // of h264 for efficiency, but encoding either in software (libaom-av1
        // etc.) runs far slower than realtime and stalls HLS
        // (fragLoadTimeOut). Pick the first preference the running ffmpeg can
        // actually encode in realtime — which now includes the hardware
        // encoders, so a client asking for av1 on an NVENC server gets it.
        let requested_video_codec = preferred_transcode_video_codec(
            &video_codecs,
            self.encoding_helper.capabilities(),
            &options,
            media_source.video_type,
        );
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
            video_bit_rate: requested_video_bitrate,
            audio_bit_rate: request.audio_bitrate,
            max_width: request.max_width,
            max_height: request.max_height,
            max_framerate: request.max_framerate,
            allow_video_stream_copy: request.allow_video_stream_copy,
            allow_audio_stream_copy: request.allow_audio_stream_copy,
            // The profile/level/framerate/size requests feed the master
            // playlist's CODECS/RESOLUTION/FRAME-RATE fields (and the level
            // clamp) exactly as `BaseEncodingJobOptions` does upstream.
            profile: request.profile.clone(),
            level: request.level.clone(),
            framerate: request.framerate,
            width: request.width,
            height: request.height,
            // `ParseStreamOptions`: every query key starting lower-case is a
            // per-codec stream option (`h264-profile`, `aac-profile`, …) — the
            // only way a real client's requested profile/level reaches
            // `GetRequestedProfiles`/`GetRequestedLevel`.
            stream_options: parse_stream_options(&request.query_string),
            // `EncodingHelper.CanStreamCopyVideo` reads this for its "for LiveTV
            // with no bitrate, try copy if other conditions are met" branch — a
            // live MPEG-TS usually probes with no bitrate, so leaving it unset
            // software-transcodes every channel that could have been copied.
            live_stream_id: request.live_stream_id.clone(),
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
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
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

        // `GetStreamingState` order: the output audio channels and bitrate are
        // resolved against the REQUESTED audio codec before `TryStreamCopy`
        // (a later copy does not recompute them — they still drive the master
        // playlist's BANDWIDTH), and the video bitrate likewise before the
        // copy decision, regardless of its outcome.
        //
        // Channels: a focused port of `GetNumAudioChannelsParam` — the
        // requested channels (folding in `TranscodingMaxAudioChannels`),
        // clamped to the source, the encoder ceiling, and the HLS layout fix.
        let audio_encoder = self.encoding_helper.audio_encoder(&probe_state);
        probe_state.output_audio_channels = resolve_output_audio_channels(
            &probe_state,
            Some(&requested_audio_codec),
            &audio_encoder,
        );
        // `LosslessAudioCodecs.Contains(outputAudioCodec)` → the source's own
        // bitrate (or 0); else `GetAudioBitrateParam(request.AudioBitRate,
        // request.AudioCodec, AudioStream, OutputAudioChannels) ?? 0`.
        probe_state.output_audio_bitrate =
            Some(if is_lossless_audio_codec(&requested_audio_codec) {
                audio_stream.as_ref().and_then(|a| a.bit_rate).unwrap_or(0)
            } else {
                self.encoding_helper
                    .audio_bitrate_param(
                        request.audio_bitrate,
                        Some(&requested_audio_codec),
                        audio_stream.as_ref(),
                        probe_state.output_audio_channels,
                    )
                    .unwrap_or(0)
            });
        // `GetVideoBitrateParamValue(VideoRequest, VideoStream, OutputVideoCodec)`
        // for every video request — a remux carries it too (it is the
        // master playlist's BANDWIDTH); the arg builder only emits bitrate args
        // on a re-encode.
        probe_state.output_video_bitrate = if is_audio {
            None
        } else {
            let value = self.encoding_helper.video_bitrate_param_value(
                &probe_state.base_request,
                probe_state.video_stream.as_ref(),
                &requested_video_codec,
            );
            (value > 0).then_some(value)
        };

        let copy_video = video_stream
            .as_ref()
            .is_some_and(|v| self.encoding_helper.can_stream_copy_video(&probe_state, v));
        // `TryStreamCopy` runs only under `if (state.VideoRequest is not null)`:
        // an audio-only HLS request never stream-copies its audio (the variant
        // URL keeps `audioCodec=aac`, and ffmpeg re-encodes).
        let copy_audio = !is_audio
            && audio_stream.as_ref().is_some_and(|a| {
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
        // Upstream's `OutputAudioCodec` is never null (it is the requested
        // codec, `"copy"` after `TryStreamCopy`), even for a source with no
        // audio stream — the master playlist's `AudioCodec` rewrite compares
        // against it. The arg builder only reads it under `audio_stream`.
        let output_audio_codec = Some(if copy_audio {
            "copy".to_owned()
        } else {
            requested_audio_codec.clone()
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
        // Bitrate-driven resolution bound — port of the `ResolutionNormalizer`
        // application in `StreamingHelpers.GetStreamingState`: a bitrate-capped
        // re-encode also bounds the output resolution (an 8 Mbps ask on a 4K
        // source downscales to 1080p, like Jellyfin), unless the requested
        // bitrate already exceeds the source's. Upstream's guard is
        // `!IsCopyCodec(OutputVideoCodec) && OutputVideoBitrate.HasValue`.
        if let Some(output_bitrate) = probe_state
            .output_video_bitrate
            .filter(|_| !is_remuxing_video)
        {
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
        // The VAAPI driver behind the configured render node, probed once per
        // device path. For every other accelerator this is the boot
        // capabilities untouched.
        let probed_caps = self.vaapi_prober.capabilities(&options).await;
        let (arguments, ffmpeg_env) = self.build_arguments(
            &state,
            &media_path,
            segment_id,
            &segment_container,
            &playlist_path,
            &options,
            burn_subtitle_path.as_deref(),
            kind,
            &probed_caps,
        );

        // ---- (5) RETURN the TranscodePlan -----------------------------------
        // `StreamState.MinSegments`: the request's value, else 2 for segments
        // of ten seconds or longer, else 3.
        let min_segments = request
            .min_segments
            .unwrap_or(if segment_length_secs >= 10 { 2 } else { 3 });
        Ok(TranscodePlan {
            state,
            playlist_path,
            arguments,
            ffmpeg_env,
            media_path,
            run_time_ticks,
            segment_length_ms: segment_length_secs.saturating_mul(MS_PER_SECOND),
            is_remuxing_video,
            segment_container,
            encoding_options: options,
            min_segments,
        })
    }
}

impl FerrofinStreamStatePlanner {
    /// The `subtitles` filter that burns a text subtitle in, in one of its two
    /// spellings. Port of `GetTextSubtitlesFilter`, with three divergences.
    ///
    /// Upstream always pre-extracts the track to its own file; Ferrofin keeps a
    /// fallback that reads it straight out of the media (`si=` selects among the
    /// embedded subtitle streams) for when no extraction is cached, which is
    /// correct but re-demuxes the source.
    ///
    /// The second is an open work item, not a choice: upstream appends
    /// `:fontsdir='{attachment folder}'` on both branches, and `:charenc=` on
    /// the external one. Ferrofin emits neither, so an ASS/SSA subtitle that
    /// ships its own fonts as attachments renders in a system font instead.
    /// Un-defer path: `GetAttachmentFolderPath(MediaSource.Id)` → `:fontsdir=`,
    /// and the subtitle encoder's detected character set → `:charenc=`.
    ///
    /// The third, the `setpts` wrapper, is Ferrofin's and is why `offset_secs`
    /// exists.
    /// Upstream emits none for HLS because it seeks once, streams, and copies
    /// timestamps, so its frames carry absolute PTS throughout. Ferrofin spawns
    /// one ffmpeg per segment with its own `-ss` and no `-copyts`, so frames
    /// arrive at PTS ≈ 0 while the filter picks cues by absolute PTS. Shifting
    /// forward for the filter and back for the muxer keeps both right.
    ///
    /// Both spellings need it, for the same reason by two routes: the plain one
    /// runs on decoder frames that restart at zero, and the `alpha`/`sub2video`
    /// one runs on an `alphasrc` source that is likewise started at zero,
    /// because a source started at the seek position would sit in a time range
    /// the video never reaches.
    fn text_subtitle_filter(
        state: &EncodingJobInfo,
        sub: &MediaStream,
        media_path: &str,
        burn_subtitle_path: Option<&str>,
        alpha_sub2video: bool,
        offset_secs: Option<i64>,
    ) -> String {
        use std::fmt::Write as _;
        let mut chain = String::new();
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
        if alpha_sub2video {
            chain.push_str(":alpha=1:sub2video=1");
        }
        if let Some(off) = offset_secs {
            let _ = write!(chain, ",setpts=PTS-{off}/TB");
        }
        chain
    }

    /// The ffmpeg video encoder for `state`. Port of `GetVideoEncoder`.
    ///
    /// One dispatch decides software and hardware alike — it returns the
    /// vendor encoder when the configured accelerator has one for the target
    /// codec and the running ffmpeg was built with it, and the software encoder
    /// otherwise. A copy request short-circuits ahead of it, since `copy` is
    /// not an encoder at all.
    fn resolve_video_encoder(&self, state: &EncodingJobInfo, options: &EncodingOptions) -> String {
        let codec = state.output_video_codec.as_deref().unwrap_or("copy");
        if EncodingJobInfo::is_copy_codec(Some(codec)) {
            return "copy".to_owned();
        }
        hw::encoder::video_encoder(
            Some(codec),
            self.encoding_helper.capabilities(),
            options.hardware_acceleration_type,
            state.media_source.video_type,
            options.enable_hardware_encoding && hardware_path_is_ported(options),
        )
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
        kind: PlaylistKind,
        probed_caps: &FfmpegCapabilities,
    ) -> (Vec<String>, Vec<(String, String)>) {
        let mut args: Vec<String> = Vec::new();
        let is_event_playlist = kind == PlaylistKind::Event;

        // Resolve the video encoder up front (it decides input hwaccel too). NVENC
        // maps the target codec to its hardware encoder; otherwise the software
        // `EncodingHelper` path (libx264 / copy) is used.
        let video_encoder = self.resolve_video_encoder(state, options);
        let copying_video = EncodingJobInfo::is_copy_codec(Some(&video_encoder));
        // The encoder is NVENC. Distinct from `hw_filters` below and used only
        // where a fact about the *encoder* is what matters.
        let nvenc_video = video_encoder.ends_with("_nvenc");
        // Everything the ported hardware matrix reads about the job. The
        // render-node arguments are VAAPI/QSV territory and unused by the CUDA
        // branch; the hardware-transcoding roadmap phase 4 resolves them from the encoding
        // options, which are not readable this early today.
        let caps = probed_caps;
        let requested = hw::decoder::RequestedSize {
            width: state.base_request.width,
            height: state.base_request.height,
            max_width: state.base_request.max_width,
            max_height: state.base_request.max_height,
        };
        let decode_ctx = hw::decoder::DecodeContext {
            caps,
            options,
            video_stream: state.video_stream.as_ref(),
            video_type: state.media_source.video_type,
            output_video_codec: state.output_video_codec.as_deref(),
            requested,
        };
        // Only NVENC has a ported filter chain today, so only NVENC may take
        // the hardware path at all. This is a **scope gate, not a capability
        // gate**: `input_video_hwaccel_args` happily produces a device graph
        // for every vendor, and `resolve_video_encoder` happily names their
        // encoders, but the chain behind them lands with phases 4-7. Letting a
        // VAAPI or QSV server through today gives it a vendor encoder with no
        // encoder parameters, no filter graph at all — so the client's
        // `MaxWidth` is silently ignored and it receives a full-resolution
        // encode — and a silently dropped subtitle burn-in. That is worse than
        // the software transcode it has now, not merely less optimal.
        //
        // Each vendor's phase removes itself from this gate as its chain and
        // its `GetEncoderParam`/`GetVideoBitrateParam` arms land.
        let hw_ported = hardware_path_is_ported(options);
        warn_unsupported_accelerator(options);
        // The configured render node, resolved against the filesystem. An
        // absent or unusable path falls through to vendor/kernel pinning
        // rather than emitting a device argument ffmpeg cannot open.
        let render_node = hw::device_init::RenderNode::resolve(options.vaapi_device.as_deref());
        let qsv_node = hw::device_init::RenderNode::resolve(options.qsv_device.as_deref());
        let hwaccel_input = if hw_ported {
            hw::input_args::input_video_hwaccel_args(
                &decode_ctx,
                &video_encoder,
                render_node,
                qsv_node,
                state.is_input_video,
            )
        } else {
            hw::input_args::InputHwaccelArgs::default()
        };
        let video_decoder = hw::decoder::hardware_video_decoder(&decode_ctx).unwrap_or_default();
        // Whether the ported vendor chain owns this job's filter graph.
        //
        // Keyed on the configured **accelerator**, which is what upstream
        // dispatches `GetVideoProcessingFilterParam` on — not on the encoder's
        // name and not on whether device arguments came out. Both of those are
        // wrong in a way that breaks real jobs:
        //
        // * Hardware *decoding* is selected for the accelerator, so it happens
        //   with a software encoder too (hardware encoding switched off, or a
        //   build with no vendor encoder). Handing those GPU frames to a
        //   software `-vf` makes ffmpeg refuse the graph outright — "Impossible
        //   to convert between the formats supported by the filter ... and
        //   auto_scale_0". The vendor chain handles that pairing itself, by
        //   downloading the frames back with `hwdownload,format=yuv420p`.
        // * A build too old for the vendor's filters produces no device
        //   arguments, but the chain still has a software fallback of its own,
        //   and that fallback is the ported one.
        let hw_filters = !copying_video && hw_ported;
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
        // MSE). This is the software path only — a job the hardware matrix
        // claimed builds its whole graph above.
        let sw_filters: Vec<String> = if !copying_video && !hw_filters {
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
        // The device graph and the hardware decoder, straight from the ported
        // matrix — `-init_hw_device`, `-filter_hw_device`, `-hwaccel` and
        // `-hwaccel_output_format` in upstream's order. Splitting on whitespace
        // is safe because nothing it produces contains any: the device
        // arguments phase 4 adds do carry paths, but they are render nodes
        // (`/dev/dri/renderD*`), which cannot contain a space.
        push_split(&mut args, &hwaccel_input.args);
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

        // ---- hardware video filters ------------------------------------------
        // The ported vendor chain owns the whole graph on the hardware path —
        // scaling, tonemapping, deinterlacing and the subtitle composite alike —
        // so the software blocks further down are skipped entirely for it.
        //
        // Built before the maps are emitted because the negative map depends on
        // its shape, which is upstream's order too
        // (`negativeMapArgs + args + videoProcessParam`).
        let mut hw_graph: Option<(&'static str, String)> = None;
        if hw_filters {
            let seek_offset = segment_id
                .filter(|&id| id > 0)
                .map(|id| i64::from(id) * i64::from(state.segment_length_secs));
            let sub_stream = state.subtitle_stream.as_ref().filter(|_| burn_sub);
            let (plain, alpha_sub2video) = sub_stream
                .filter(|_| burn_text)
                .map(|sub| {
                    (
                        Self::text_subtitle_filter(
                            state,
                            sub,
                            media_path,
                            burn_subtitle_path,
                            false,
                            seek_offset,
                        ),
                        Self::text_subtitle_filter(
                            state,
                            sub,
                            media_path,
                            burn_subtitle_path,
                            true,
                            seek_offset,
                        ),
                    )
                })
                .unwrap_or_default();
            let subtitle = if burn_text {
                hw::sw_chain::SubtitleOverlay::Text {
                    plain: &plain,
                    alpha_sub2video: &alpha_sub2video,
                    is_ass: sub_stream
                        .and_then(|s| s.codec.as_deref())
                        .is_some_and(|c| {
                            c.eq_ignore_ascii_case("ass") || c.eq_ignore_ascii_case("ssa")
                        }),
                }
            } else if let Some(sub) = sub_stream.filter(|_| burn_graphical) {
                hw::sw_chain::SubtitleOverlay::Graphical {
                    width: sub.width,
                    height: sub.height,
                }
            } else {
                hw::sw_chain::SubtitleOverlay::None
            };
            let video_stream = state.video_stream.as_ref();
            let chain_input = hw::sw_chain::ChainInput {
                caps,
                options,
                video_encoder: &video_encoder,
                video_decoder: &video_decoder,
                video_width: video_stream.and_then(|v| v.width),
                video_height: video_stream.and_then(|v| v.height),
                requested,
                three_d_format: state.media_source.video3d_format,
                rotation: video_stream.and_then(|v| v.rotation),
                color_transfer: video_stream.and_then(|v| v.color_transfer.as_deref()),
                reference_frame_rate: video_stream.and_then(MediaStream::reference_frame_rate),
                real_frame_rate: video_stream.and_then(|v| v.real_frame_rate),
                // Zero, NOT the seek position — see `text_subtitle_filter`.
                // Upstream can start `alphasrc` at the seek because its HLS
                // copies timestamps, so its decoded frames keep absolute PTS.
                // Ferrofin seeks per segment without `-copyts`, so decoded
                // frames restart at ~0 (verified: `-ss 3` gives pts_time 0,
                // and 3 only with `-copyts`). Starting the generated source at
                // the seek would put it in a time range the video never
                // reaches, and nothing would be drawn at all.
                start_time_ticks: 0,
                // `doDeintH264 || doDeintHevc` — upstream asks under each
                // spelling of the two codecs it deinterlaces.
                deinterlace: ["h264", "avc", "h265", "hevc"]
                    .iter()
                    .any(|c| state.deinterlace(Some(c), true)),
                // Computed, not assumed: the vendor chain falls back to the
                // software chain on a build without the vendor's tonemap
                // filter, and that fallback is the only tonemap an HDR source
                // gets there — the planner's own software tonemap block is
                // skipped for a job the matrix claimed.
                do_sw_tonemap: hw::tonemap::is_sw_tonemap_available(caps, video_stream),
                do_hw_tonemap: hw::input_args::is_hw_tonemap_available(&decode_ctx, &video_decoder),
                // Read by the VAAPI chain only, which prefers Intel's VPP
                // tonemap and falls back to the OpenCL one.
                vulkan_tonemap_available: hw::tonemap::is_vulkan_hw_tonemap_available(
                    options,
                    video_stream,
                ),
                vpp_tonemap_available: hw::tonemap::is_intel_vpp_tonemap_available(
                    caps,
                    options,
                    video_stream,
                ),
                source_codec: video_stream.and_then(|v| v.codec.as_deref()),
                is_dovi: hw::tonemap::is_dovi(video_stream),
                is_hevc_rext: hw::decoder::is_video_stream_hevc_rext(video_stream),
                subtitle,
            };
            // `find_index`, not the stream's own `index` — the pads have to
            // name the same numbers `map_args` does, and the two diverge as soon
            // as a source's streams are not contiguously indexed. It matters
            // most for an external subtitle, which is input 1 with a single
            // stream at index 0 whatever its index in the parent source.
            let streams = &state.media_source.media_streams;
            let pads = hw::sw_chain::StreamPads {
                subtitle_is_external: sub_stream.is_some_and(|s| s.is_external),
                subtitle_index: sub_stream.map_or(0, |s| {
                    ferrofin_mediaencoding::encoding_helper::helper::find_index(streams, s)
                }),
                video_index: video_stream.map_or(0, |v| {
                    ferrofin_mediaencoding::encoding_helper::helper::find_index(streams, v)
                }),
            };
            // Upstream dispatches `GetVideoProcessingFilterParam` on the
            // accelerator, and each vendor chain falls back to the software one
            // itself when its own pipeline cannot run.
            let vendor_chain = match options.hardware_acceleration_type {
                HardwareAccelerationType::vaapi => hw::vaapi::vaapi_vid_filter_chain(&chain_input),
                HardwareAccelerationType::qsv => hw::qsv::intel_vid_filter_chain(&chain_input),
                _ => hw::nvidia::nvidia_vid_filter_chain(&chain_input),
            };
            hw_graph = hw::sw_chain::video_processing_filter_args(
                vendor_chain,
                self.encoding_helper.framerate_param(state).map(f64::from),
                pads,
                burn_sub,
                burn_text,
            );
        }

        // ---- map -------------------------------------------------------------
        // The negative map cancels the positively-mapped video whenever the
        // graph is a `-filter_complex`, because ffmpeg adds that graph's
        // unlabeled output to the muxer by itself. Without it the output
        // carries the raw video AND the filtered one.
        let hw_graph_flag = hw_graph.as_ref().map_or("", |(flag, _)| *flag);
        push_split(&mut args, &self.encoding_helper.map_args(state, hw_filters));
        // After the positive maps, which is where upstream puts it too: C#
        // prepends it to the *video arguments* fragment, and that fragment
        // lands after `GetMapArgs` in the command-line template, so both emit
        // `-map 0:v -map 0:a -map -0:v -codec:v:0 …`. The order is not
        // cosmetic — ffmpeg rejects a leading negative map outright ("Stream
        // map '' matches no streams"), since it can only subtract from a set
        // that already exists.
        push_split(
            &mut args,
            &ferrofin_mediaencoding::encoding_helper::helper::negative_map_args_by_filters(
                state,
                hw_graph_flag,
            ),
        );
        if let Some((flag, graph)) = hw_graph {
            args.push(flag.to_owned());
            args.push(graph);
        }

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
        // ---- bitstream filters -----------------------------------------------
        // Only on a stream copy: a re-encode produces a fresh bitstream that
        // needs neither the container fixup nor the metadata strip. This is what
        // lets a Dolby Vision file copy through to a client that cannot play it
        // — the alternative is re-encoding a 4K HDR source.
        //
        // The `NalLengthSize == "0"` gate is upstream's and it suppresses the
        // metadata removal as well as the `mp4toannexb` conversion: a source
        // already in Annex B form gets no `-bsf:v` at all, so its Dolby Vision
        // RPU survives even when the client asked for it gone.
        if copying_video
            && state
                .video_stream
                .as_ref()
                .is_some_and(|v| v.nal_length_size.as_deref() != Some("0"))
            && let Some(bsf) = ferrofin_mediaencoding::encoding_helper::bitstream::bit_stream_args(
                caps,
                state,
                MediaStreamType::Video,
            )
        {
            push_split(&mut args, &bsf);
        }
        if !copying_video {
            // One quality path for every encoder. `video_quality_param` carries
            // the preset, the bitrate (`-b:v`/`-maxrate`/`-bufsize`) and `-r`
            // together, mirroring `GetVideoQualityParam` — pushing any of them
            // again here would hand ffmpeg the same flag twice.
            // Upstream computes these inside `GetVideoQualityParam`, ahead of
            // everything else it emits; they are split out because deciding
            // them needs the concrete capabilities.
            push_split(
                &mut args,
                &hw::quality::hardware_quality_preamble(&decode_ctx, options, &video_encoder),
            );
            push_split(
                &mut args,
                &self.encoding_helper.video_quality_param(
                    state,
                    &video_encoder,
                    options,
                    if is_event_playlist {
                        DEFAULT_EVENT_ENCODER_PRESET
                    } else {
                        DEFAULT_ENCODER_PRESET
                    },
                ),
            );
            let output_framerate = self.encoding_helper.framerate_param(state);
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
        // `GetVideoArguments`' "TODO why was this not enabled for VOD?": an
        // event playlist in mpegts segments drops the global header (copy and
        // re-encode alike). Keyed on the NORMALISED container, as upstream's
        // `outputExtension.TrimStart('.')` is — an unknown container falls back
        // to mpegts there and must here too.
        if is_event_playlist && segment_file_extension(segment_container) == "ts" {
            push_split(&mut args, "-flags -global_header");
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
            && !hw_filters
            && let Some(sub) = state.subtitle_stream.as_ref()
        {
            use std::fmt::Write as _;
            let mut chain = String::new();
            // Downscale/tonemap ahead of the subtitle render, so the text is
            // drawn at output resolution in SDR.
            for filter in &sw_filters {
                let _ = write!(chain, "{filter},");
            }
            let offset_secs = segment_id
                .filter(|&id| id > 0)
                .map(|id| i64::from(id) * i64::from(state.segment_length_secs));
            chain.push_str(&Self::text_subtitle_filter(
                state,
                sub,
                media_path,
                burn_subtitle_path,
                false,
                offset_secs,
            ));
            args.push("-vf".to_owned());
            args.push(chain);
        }

        // ---- graphical subtitle burn-in --------------------------------------
        // Composite the (bitmap) subtitle stream onto the video: `overlay` takes
        // the base video and the decoded subtitle as its two inputs and emits the
        // labelled `[v]` that `map_args` maps in place of the raw video.
        if burn_graphical
            && !hw_filters
            && let Some(sub) = state.subtitle_stream.as_ref()
        {
            let sub_idx = sub.index.max(0);
            let vid_idx = state.video_stream.as_ref().map_or(0, |v| v.index.max(0));
            // No hardware format conversion here: this block runs only when
            // the matrix did not claim the job, so the frames are in system
            // memory and the software chain above has already set the format.
            push_split(&mut args, "-filter_complex");
            if sw_filters.is_empty() {
                args.push(format!("[0:{vid_idx}][0:{sub_idx}]overlay[v]"));
            } else {
                // Downscale/tonemap the base video before compositing, so the
                // bitmap subtitle is overlaid in SDR at output resolution.
                args.push(format!(
                    "[0:{vid_idx}]{}[base];[base][0:{sub_idx}]overlay[v]",
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
        args.push(if is_event_playlist { "event" } else { "vod" }.to_owned());
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
        // An event playlist is served as ffmpeg wrote it, so its segment URIs
        // must already point at the `hls/{playlistId}/{segment}` route
        // (`GetCommandLineArguments`' `-hls_base_url "hls/{0}/"`).
        if is_event_playlist {
            push_split(&mut args, "-hls_base_url");
            args.push(format!("hls/{stem}/"));
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

        (args, hwaccel_input.env)
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
    // Upstream keys the output path on `state.MediaPath`, which for a live
    // stream is the per-open buffer file and so differs on every tune. Without
    // this, re-tuning a channel on the same play-session/device tuple collides
    // with the previous tune and replays its segments.
    request.live_stream_id.hash(&mut hasher);
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
    use std::collections::HashMap;
    use uuid::Uuid;

    /// A fake [`MediaSourceManager`] returning a fixed source list, plus the
    /// open live streams `get_live_stream` can hand back.
    #[derive(Default)]
    struct FakeMediaSources {
        sources: Vec<MediaSourceInfo>,
        live_streams: HashMap<String, MediaSourceInfo>,
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
        async fn get_live_stream(&self, id: &str) -> Result<MediaSourceInfo, ServiceError> {
            // The real manager 404s a closed/unknown id rather than falling
            // back to the item's static sources.
            self.live_streams
                .get(id)
                .cloned()
                .ok_or_else(|| ServiceError::not_found("live stream is not open"))
        }
        async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    // The ffmpeg path the planner hands its VAAPI prober. Tests that need a
    // specific probe result point this at a stub script; everything else
    // leaves it as `ffmpeg`, which fails to open any render node and so leaves
    // every VAAPI flag clear.
    thread_local! {
        static FAKE_FFMPEG: std::cell::RefCell<String> =
            const { std::cell::RefCell::new(String::new()) };
    }

    /// A fake [`MediaEncoder`]: only the arg-building methods are exercised.
    struct FakeEncoder;

    #[async_trait]
    impl MediaEncoder for FakeEncoder {
        fn encoder_path(&self) -> String {
            FAKE_FFMPEG.with(|p| {
                let p = p.borrow();
                if p.is_empty() {
                    "ffmpeg".to_owned()
                } else {
                    p.clone()
                }
            })
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

    /// A planner whose fake media-source manager also has `id` open as a live
    /// stream, alongside the static `sources`.
    fn planner_with_live_stream(
        sources: Vec<MediaSourceInfo>,
        id: &str,
        live: MediaSourceInfo,
    ) -> FerrofinStreamStatePlanner {
        let mut live_streams = HashMap::new();
        live_streams.insert(id.to_owned(), live);
        planner_over(
            Arc::new(FakeMediaSources {
                sources,
                live_streams,
            }),
            false,
            &[],
        )
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
        planner_over(
            Arc::new(FakeMediaSources {
                sources,
                live_streams: HashMap::new(),
            }),
            supports_tonemapx,
            encoders,
        )
    }

    /// [`planner_full`] over a caller-supplied fake, for the tests that need
    /// open live streams as well as static sources.
    fn planner_over(
        media_sources: Arc<dyn MediaSourceManager>,
        supports_tonemapx: bool,
        encoders: &[&str],
    ) -> FerrofinStreamStatePlanner {
        planner_over_with(
            media_sources,
            supports_tonemapx,
            encoders,
            EncodingOptions::default(),
        )
    }

    /// The same planner with explicit encoding options, so a test can select a
    /// hardware accelerator the way the dashboard does.
    fn planner_over_with(
        media_sources: Arc<dyn MediaSourceManager>,
        supports_tonemapx: bool,
        encoders: &[&str],
        options: EncodingOptions,
    ) -> FerrofinStreamStatePlanner {
        planner_over_caps(
            media_sources,
            supports_tonemapx,
            FfmpegCapabilities::builder()
                .encoders(encoders.iter().copied())
                .build(),
            options,
        )
    }

    /// The planner over an explicit capability probe — what a test needs when
    /// the hardware path depends on more than the encoder list.
    fn planner_over_caps(
        media_sources: Arc<dyn MediaSourceManager>,
        supports_tonemapx: bool,
        caps: FfmpegCapabilities,
        options: EncodingOptions,
    ) -> FerrofinStreamStatePlanner {
        let encoder: Arc<dyn MediaEncoder> = Arc::new(FakeEncoder);
        let helper = EncodingHelper::with_processor_count(caps, 8);
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            "/data",
            std::path::PathBuf::from("/data/log"),
            "/config",
            "/cache",
            "/web",
        ));
        let config: Arc<dyn ServerConfigurationManager> =
            Arc::new(FakeConfig(Arc::clone(&paths), options));
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
    struct FakeConfig(Arc<FerrofinServerApplicationPaths>, EncodingOptions);

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
            Ok(self.1.clone())
        }
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
        let plan = p
            .plan(&request("abc"), false, None, PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();

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

    /// An ffmpeg build with every NVENC encoder, so the ported dispatch has
    /// something to find. Selection is a probe result now, not a fixed table:
    /// a build without `av1_nvenc` must fall back even with NVENC configured.
    fn nvenc_caps() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .encoders(["h264_nvenc", "hevc_nvenc", "av1_nvenc", "libx264"])
            .build()
    }

    #[test]
    fn transcode_target_skips_codecs_ferrofin_cannot_encode() {
        // Software-only: browsers list av1/vp9 first, but only libx264 (h264) is
        // realtime-viable — picking av1 would launch libaom-av1 and stall HLS.
        let sw = EncodingOptions::default();
        let av1_first = vec!["av1".to_owned(), "h264".to_owned(), "vp9".to_owned()];
        assert_eq!(
            preferred_transcode_video_codec(&av1_first, &nvenc_caps(), &sw, None),
            "h264"
        );
        // A bare `copy` request is honoured verbatim.
        assert_eq!(
            preferred_transcode_video_codec(&["copy".to_owned()], &nvenc_caps(), &sw, None),
            "copy"
        );
        // No encodable preference (or none at all) → the h264 default.
        assert_eq!(
            preferred_transcode_video_codec(
                &["av1".to_owned(), "vp9".to_owned()],
                &nvenc_caps(),
                &sw,
                None,
            ),
            "h264"
        );
        assert_eq!(
            preferred_transcode_video_codec(&[], &nvenc_caps(), &sw, None),
            "h264"
        );
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
        assert_eq!(
            preferred_transcode_video_codec(&av1_first, &nvenc_caps(), &nv, None),
            "av1"
        );
        // vp9 has no NVENC encoder → skipped in favour of the next encodable.
        assert_eq!(
            preferred_transcode_video_codec(
                &["vp9".to_owned(), "hevc".to_owned()],
                &nvenc_caps(),
                &nv,
                None,
            ),
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p
            .plan(&request("abc"), false, None, PlaylistKind::Vod)
            .await
            .unwrap();

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

    /// The buffered tuner copy a live stream resolves to: an infinite MPEG-TS
    /// whose bitrate ffmpeg could not probe, carrying the tuner's fallback cap.
    fn live_source(id: &str) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some(id.to_owned()),
            path: Some("/transcodes/tuner-buffer.ts".to_owned()),
            container: Some("ts".to_owned()),
            is_infinite_stream: true,
            run_time_ticks: None,
            fallback_max_streaming_bitrate: Some(30_000_000),
            media_streams: vec![video_stream("h264"), audio_stream("aac")],
            ..MediaSourceInfo::default()
        }
    }

    #[tokio::test]
    async fn plan_prefers_the_open_live_stream_over_the_static_source() {
        // `StreamingHelpers.GetStreamingState` puts the whole media-source-id
        // resolution inside `if (string.IsNullOrWhiteSpace(LiveStreamId))`.
        // Re-resolving the channel's static source here would dial the tuner a
        // second time while one is already open.
        let stale = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner_with_live_stream(
            vec![stale],
            "prov_service_source",
            live_source("prov_service_source"),
        );

        let mut req = request("abc");
        req.live_stream_id = Some("prov_service_source".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.media_path, "/transcodes/tuner-buffer.ts");

        // Without the id the static source still wins — the very same fake, so
        // this is the branch and not a different planner.
        let plan = p
            .plan(&request("abc"), false, None, PlaylistKind::Vod)
            .await
            .unwrap();
        assert_eq!(plan.media_path, "/media/movie.mkv");
    }

    #[tokio::test]
    async fn plan_reports_a_closed_live_stream_rather_than_falling_back() {
        // A tuner that has been closed (or an id a client invented) must not
        // quietly become "transcode the item's file instead" — that would dial
        // the tuner again behind the client's back.
        let p = planner_with_live_stream(
            vec![source(
                "abc",
                vec![video_stream("h264"), audio_stream("aac")],
            )],
            "open",
            live_source("open"),
        );
        let mut req = request("abc");
        req.live_stream_id = Some("closed".to_owned());
        let result = p.plan(&req, false, None, PlaylistKind::Vod).await;
        assert!(matches!(result, Err(ServiceError::NotFound(_))));

        // Whitespace is `IsNullOrWhiteSpace` upstream: it means "no live
        // stream", not "a live stream named a space".
        let mut req = request("abc");
        req.live_stream_id = Some("   ".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.media_path, "/media/movie.mkv");
    }

    #[tokio::test]
    async fn plan_caps_the_requested_bitrate_at_the_live_source_fallback() {
        // A client's "auto" ceiling is far above what the tuner delivers.
        // Upstream caps the ask against `FallbackMaxStreamingBitrate`;
        // uncapped it flows into `-maxrate` and suppresses the downscale.
        // Give the source a probed bitrate so the copy path is out of the way
        // and the number really does reach the encoder args.
        let mut live = live_source("prov_service_source");
        live.media_streams[0].bit_rate = Some(40_000_000);
        let p = planner_with_live_stream(Vec::new(), "prov_service_source", live);

        let mut req = request("abc");
        req.live_stream_id = Some("prov_service_source".to_owned());
        req.video_bitrate = Some(140_000_000);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            !args.contains("140000000"),
            "the uncapped ask must not reach ffmpeg: {args}"
        );
        assert!(args.contains("30000000"), "capped at the fallback: {args}");

        // An ask below the fallback is left alone.
        let mut req = request("abc");
        req.live_stream_id = Some("prov_service_source".to_owned());
        req.video_bitrate = Some(3_000_000);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(
            plan.arguments.join(" ").contains("3000000"),
            "{:?}",
            plan.arguments
        );
    }

    #[tokio::test]
    async fn plan_lets_an_unprobed_live_stream_stream_copy() {
        // `EncodingHelper.CanStreamCopyVideo`: "for LiveTV with no bitrate, try
        // copy if other conditions are met" — gated on `live_stream_id` being
        // set on the job options. A live MPEG-TS usually probes with no
        // bitrate, so without the field every channel a client could have
        // direct-played gets a full software transcode instead.
        let p = planner_with_live_stream(
            vec![source(
                "abc",
                vec![video_stream("h264"), audio_stream("aac")],
            )],
            "prov_service_source",
            live_source("prov_service_source"),
        );
        let mut req = request("abc");
        req.live_stream_id = Some("prov_service_source".to_owned());
        req.video_bitrate = Some(3_000_000);
        req.video_codec = Some("h264".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(
            plan.arguments.join(" ").contains("-c:v copy"),
            "{:?}",
            plan.arguments
        );

        // The same unprobed source WITHOUT an open live stream is an ordinary
        // file: the ask below the (unknown) source bitrate vetoes the copy.
        let mut unprobed = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        unprobed.container = Some("ts".to_owned());
        let p = planner(vec![unprobed]);
        let mut req = request("abc");
        req.video_bitrate = Some(3_000_000);
        req.video_codec = Some("h264".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(
            !plan.arguments.join(" ").contains("-c:v copy"),
            "{:?}",
            plan.arguments
        );
    }

    #[test]
    fn output_id_separates_two_tunes_of_the_same_channel() {
        // Upstream keys the output path on the media path, which for a live
        // stream is the per-open buffer file. Two tunes on one
        // play-session/device tuple must not share a transcode directory, or
        // the second replays the first tune's segments.
        let mut first = request("abc");
        first.live_stream_id = Some("prov_service_a".to_owned());
        let mut second = request("abc");
        second.live_stream_id = Some("prov_service_b".to_owned());
        assert_ne!(
            output_id(&first, "ts", false),
            output_id(&second, "ts", false)
        );
    }

    #[tokio::test]
    async fn plan_missing_source_is_not_found() {
        let src = source("abc", vec![video_stream("h264")]);
        let p = planner(vec![src]);
        let result = p
            .plan(&request("nope"), false, None, PlaylistKind::Vod)
            .await;
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
            let plan = p
                .plan(&request(id), false, None, PlaylistKind::Vod)
                .await
                .unwrap();
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
                PlaylistKind::Vod,
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&req, false, Some(3), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&request("abc"), false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
            let plan = p
                .plan(&req, false, Some(0), PlaylistKind::Vod)
                .await
                .unwrap();
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();

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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&request("abc"), false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();

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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
        assert_eq!(plan.segment_length_ms, 6000);
        assert!(
            plan.arguments.windows(2).any(|w| w == ["-hls_time", "6"]),
            "{:?}",
            plan.arguments
        );

        // Absent → the 3s default.
        let plan = p
            .plan(&request("abc"), false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
        assert_eq!(plan.segment_length_ms, 3000);

        // A degenerate 0 would make the playlist generator divide by zero
        // (`HlsError::InvalidOperation` → 500); it falls back to the default
        // instead. Deliberate divergence: upstream takes the 0 verbatim.
        let mut req = request("abc");
        req.segment_length = Some(0);
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
        assert_eq!(plan.segment_length_ms, 3000);
    }

    #[tokio::test]
    async fn plan_builds_hls_muxer_args() {
        let src = source("abc", vec![video_stream("h264"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let plan = p
            .plan(&request("abc"), false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&request("abc"), true, Some(2), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&req, false, Some(2), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&request("abc"), false, Some(2), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p
            .plan(&request("abc"), false, None, PlaylistKind::Vod)
            .await
            .unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(
            plan.arguments.join(" ").contains("-c:v copy"),
            "control: copy expected when allowed"
        );

        let p = planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("hevc,h264".to_owned());
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
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
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(
            args.contains("scale=1920:1080"),
            "bitrate-driven downscale expected: {args}"
        );
        assert!(args.contains("-maxrate 8000000"), "cap expected: {args}");
    }

    /// An 8-bit 1080p H.264 source. The pixel format is load-bearing: NVDEC is
    /// selected per format, and upstream declines hardware decoding outright
    /// when the probe reported none.
    fn nvdec_decodable_video_stream() -> MediaStream {
        MediaStream {
            pixel_format: Some("yuv420p".to_owned()),
            width: Some(1920),
            height: Some(1080),
            ..video_stream("h264")
        }
    }

    /// A real CUDA build: the encoders alone are not enough, the chain also
    /// needs the hwaccel and the CUDA filters to be present.
    fn nvenc_caps_full() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(ferrofin_mediaencoding::encoding_helper::hw::Platform::Linux)
            .encoders(["h264_nvenc", "hevc_nvenc", "av1_nvenc", "libx264"])
            .hwaccels(["cuda"])
            .filters(ferrofin_mediaencoding::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(ferrofin_mediaencoding::encoder::FfmpegVersion::with_build(
                7, 0, 1,
            ))
            .build()
    }

    /// A planner with NVENC selected and an ffmpeg that has the encoders.
    fn nvenc_planner(sources: Vec<MediaSourceInfo>) -> FerrofinStreamStatePlanner {
        planner_over_caps(
            Arc::new(FakeMediaSources {
                sources,
                live_streams: HashMap::new(),
            }),
            false,
            nvenc_caps_full(),
            EncodingOptions {
                enable_hardware_encoding: true,
                hardware_acceleration_type: HardwareAccelerationType::nvenc,
                encoder_preset: EncoderPreset::medium,
                ..EncodingOptions::default()
            },
        )
    }

    #[tokio::test]
    async fn nvenc_now_takes_jellyfins_argument_shapes() {
        // These replace Ferrofin's former bespoke NVENC arguments, which had no
        // device initialisation and a hand-rolled `-vf scale_cuda`. Everything
        // here now comes from the ported matrix.
        let src = source(
            "abc",
            vec![nvdec_decodable_video_stream(), audio_stream("aac")],
        );
        let p = nvenc_planner(vec![src]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.video_bitrate = Some(7_616_000);
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");

        assert!(args.contains("-c:v h264_nvenc"), "{args}");
        // The whole pipeline stays on the card: a device is created, the
        // decoder writes into it, and the frames never come back to memory.
        // Ferrofin previously passed a bare `-hwaccel cuda` with no device at
        // all, and no `-hwaccel_flags`.
        assert!(
            args.contains(
                "-init_hw_device cuda=cu:0 -filter_hw_device cu -hwaccel cuda -hwaccel_output_format cuda"
            ),
            "{args}"
        );
        assert!(args.contains("-hwaccel_flags +unsafe_output"), "{args}");
        // The CUDA scaler, not the software one.
        assert!(args.contains("-vf setparams="), "{args}");
        assert!(args.contains("scale_cuda=format=yuv420p"), "{args}");
        assert!(!args.contains("hwdownload"), "{args}");
        // NVENC's own preset ladder: p4 for `medium`, not an x264 name.
        assert!(args.contains("-preset p4"), "{args}");
        // The generic bitrate shape — targeted, not merely capped.
        assert!(
            args.contains("-b:v 7616000 -maxrate 7616000 -bufsize 15232000"),
            "{args}"
        );
        // Gone with the bolt-on: the fixed constant-quality target and `-crf`.
        assert!(!args.contains("-cq"), "{args}");
        assert!(!args.contains("-crf"), "{args}");
    }

    /// This machine's actual ffmpeg n9.0.1: the CUDA filters are present but
    /// `tonemap_cuda` and `alphasrc` are not — they are jellyfin-ffmpeg
    /// additions. `IsCudaFullSupported` therefore fails and upstream degrades
    /// to GPU encode only, which is why Jellyfin ships its own ffmpeg build.
    #[tokio::test]
    async fn a_stock_ffmpeg_degrades_to_gpu_encode_only() {
        let stock = FfmpegCapabilities::builder()
            .platform(ferrofin_mediaencoding::encoding_helper::hw::Platform::Linux)
            .encoders(["h264_nvenc", "hevc_nvenc", "av1_nvenc", "libx264"])
            .hwaccels(["cuda", "vaapi", "qsv", "vulkan", "opencl"])
            .filters(
                ferrofin_mediaencoding::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "tonemap_cuda" && *f != "alphasrc"),
            )
            .all_filter_options(true)
            // The option probe is separate from the filter list, so an absent
            // filter has to be said twice to be modelled honestly.
            .filter_option(
                ferrofin_mediaencoding::encoding_helper::hw::FilterOption::TonemapCudaName,
                false,
            )
            .ffmpeg_version(ferrofin_mediaencoding::encoder::FfmpegVersion::with_build(
                9, 0, 1,
            ))
            .build();
        let p = planner_over_caps(
            Arc::new(FakeMediaSources {
                sources: vec![source(
                    "abc",
                    vec![nvdec_decodable_video_stream(), audio_stream("aac")],
                )],
                live_streams: HashMap::new(),
            }),
            false,
            stock,
            EncodingOptions {
                enable_hardware_encoding: true,
                hardware_acceleration_type: HardwareAccelerationType::nvenc,
                encoder_preset: EncoderPreset::medium,
                ..EncodingOptions::default()
            },
        );
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.max_width = Some(1280);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        // GPU encode is kept...
        assert!(args.contains("-c:v h264_nvenc -preset p4"), "{args}");
        // ...but nothing is asked of a device that cannot do the filtering:
        // no device graph, no hardware decode, no CUDA filter.
        assert!(!args.contains("-init_hw_device"), "{args}");
        assert!(!args.contains("-hwaccel"), "{args}");
        assert!(!args.contains("_cuda"), "{args}");
        assert!(args.contains("-vf setparams="), "{args}");
        assert!(args.contains("format=yuv420p"), "{args}");
    }

    #[tokio::test]
    async fn the_software_graphical_burn_still_maps_its_own_labelled_output() {
        // The other direction of `map_args`'s switch. Ferrofin's software
        // overlay graph DOES label its output, so this path must keep asking
        // for `[v]` — dropping it here fails live with "Output with label 'v'
        // does not exist in any defined filter graph".
        let p = planner(vec![source(
            "abc",
            vec![
                nvdec_decodable_video_stream(),
                audio_stream("aac"),
                subtitle_stream("pgssub", 2),
            ],
        )]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.subtitle_stream_index = Some(2);
        req.subtitle_method = Some("Encode".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-map [v]"), "{args}");
        assert!(args.contains("[0:0][0:2]overlay[v]"), "{args}");
        // ...and no negative map, because this graph's output IS labelled.
        assert!(!args.contains("-map -0:0"), "{args}");
    }

    #[tokio::test]
    async fn filter_pads_use_the_mapped_index_not_the_streams_own() {
        // ffmpeg numbers streams by position, so a source whose stream indices
        // are not contiguous makes the two disagree. Every other fixture here
        // is contiguous, which hides it.
        // Every stream's own index differs from its position, including the
        // video's — a fixture where the video sits at 0 makes the two spellings
        // agree for it and hides half the question.
        let mut video = nvdec_decodable_video_stream();
        video.index = 2;
        let mut audio = audio_stream("aac");
        audio.index = 5;
        let mut sub = subtitle_stream("pgssub", 9);
        sub.index = 9;
        let p = nvenc_planner(vec![source("abc", vec![video, audio, sub])]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.subtitle_stream_index = Some(9);
        req.subtitle_method = Some("Encode".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        // The subtitle is the third stream, so its pad is 2 — not 9.
        assert!(args.contains("[0:2]scale,scale="), "{args}");
        assert!(!args.contains("[0:9]"), "{args}");
        // The video pad and the negative map agree with each other, and both
        // use the position (0) rather than the video's own index (2).
        assert!(args.contains("[0:0]setparams="), "{args}");
        assert!(args.contains("-map -0:0"), "{args}");
        assert!(!args.contains("-map -0:2"), "{args}");
    }

    #[tokio::test]
    async fn an_unsupported_accelerator_keeps_the_software_transcode() {
        // The accelerators Ferrofin does not support. Naming a vendor encoder
        // without its chain gives no preset, no rate control, no scaler and a
        // silently dropped subtitle — strictly worse than the software
        // transcode these servers get instead. Unsupported by decision, not by
        // omission: without the hardware to verify them on, an unverified
        // hardware pipeline is how you ship silent green frames.
        for accel in [
            HardwareAccelerationType::amf,
            HardwareAccelerationType::videotoolbox,
            HardwareAccelerationType::rkmpp,
            HardwareAccelerationType::v4l2m2m,
        ] {
            let caps = FfmpegCapabilities::builder()
                .platform(ferrofin_mediaencoding::encoding_helper::hw::Platform::Linux)
                .encoders([
                    "libx264",
                    "h264_vaapi",
                    "h264_qsv",
                    "h264_amf",
                    "h264_rkmpp",
                ])
                .hwaccels(["cuda", "vaapi", "qsv", "d3d11va", "opencl", "vulkan"])
                .filters(ferrofin_mediaencoding::encoder::REQUIRED_FILTERS)
                .all_filter_options(true)
                .ffmpeg_version(ferrofin_mediaencoding::encoder::FfmpegVersion::with_build(
                    7, 0, 1,
                ))
                .build();
            let p = planner_over_caps(
                Arc::new(FakeMediaSources {
                    sources: vec![source(
                        "abc",
                        vec![nvdec_decodable_video_stream(), audio_stream("aac")],
                    )],
                    live_streams: HashMap::new(),
                }),
                false,
                caps,
                EncodingOptions {
                    enable_hardware_encoding: true,
                    hardware_acceleration_type: accel,
                    ..EncodingOptions::default()
                },
            );
            let mut req = request("abc");
            req.video_codec = Some("h264".to_owned());
            req.allow_video_stream_copy = false;
            req.max_width = Some(1280);
            let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
            let args = plan.arguments.join(" ");
            assert!(args.contains("-c:v libx264"), "{accel:?}: {args}");
            // The bound the client asked for still reaches the scaler...
            assert!(args.contains("scale="), "{accel:?}: {args}");
            // ...and no half-wired device pipeline is emitted.
            assert!(!args.contains("-init_hw_device"), "{accel:?}: {args}");
            assert!(!args.contains("-hwaccel"), "{accel:?}: {args}");
        }
    }

    /// Writes an ffmpeg stub that reports `driver` on stderr and points the
    /// planner's VAAPI prober at it.
    fn stub_vaapi_ffmpeg(dir: &std::path::Path, driver: &str) {
        let path = dir.join("stub-ffmpeg");
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho 'VAAPI driver: {driver}' >&2\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        FAKE_FFMPEG.with(|p| *p.borrow_mut() = path.to_string_lossy().into_owned());
    }

    fn vaapi_caps() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(ferrofin_mediaencoding::encoding_helper::hw::Platform::Linux)
            .encoders(["h264_vaapi", "hevc_vaapi", "libx264"])
            .hwaccels(["vaapi", "drm", "opencl", "vulkan"])
            .filters(ferrofin_mediaencoding::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .os_version(ferrofin_mediaencoding::encoder::FfmpegVersion::new(6, 1))
            .ffmpeg_version(ferrofin_mediaencoding::encoder::FfmpegVersion::with_build(
                7, 0, 1,
            ))
            .build()
    }

    fn vaapi_options() -> EncodingOptions {
        EncodingOptions {
            enable_hardware_encoding: true,
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            // `/dev/null` rather than a real `/dev/dri/renderD*`: the node is
            // resolved with `fs::metadata`, so naming a real render node makes
            // the expected argument depend on whether the machine running the
            // tests has a GPU. It does not on CI. `/dev/null` is a character
            // device, the same class of node, and exists everywhere.
            vaapi_device: Some("/dev/null".to_owned()),
            enable_intel_low_power_h264_hw_encoder: true,
            hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
            ..EncodingOptions::default()
        }
    }

    #[tokio::test]
    async fn a_probed_ihd_device_reaches_the_command_line() {
        // The probe is the ONLY source of the driver, and this is what
        // connects it: without the prober wired in, the planner reads boot
        // capabilities where every VAAPI flag is false, and every Intel server
        // silently takes the unknown-vendor branch.
        let dir = tempfile::tempdir().unwrap();
        stub_vaapi_ffmpeg(dir.path(), "Intel iHD driver");
        let p = planner_over_caps(
            Arc::new(FakeMediaSources {
                sources: vec![source(
                    "abc",
                    vec![nvdec_decodable_video_stream(), audio_stream("aac")],
                )],
                live_streams: HashMap::new(),
            }),
            false,
            vaapi_caps(),
            vaapi_options(),
        );
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.max_width = Some(1280);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");

        // The configured render node reaches the device argument -- it was
        // empty until the node was resolved from the options.
        assert!(
            args.contains("-init_hw_device vaapi=va:/dev/null,driver=iHD"),
            "{args}"
        );
        assert!(args.contains("-c:v h264_vaapi"), "{args}");
        assert!(
            args.contains("scale_vaapi=w=1280:h=720:format=nv12"),
            "{args}"
        );
        // Intel's low-power encoder block, which only iHD/i965 have.
        assert!(args.contains("-low_power 1"), "{args}");
    }

    #[tokio::test]
    async fn an_unrecognised_vaapi_driver_gets_no_device_and_no_low_power() {
        // Real hardware lands here: NVIDIA's VAAPI shim reports a driver name
        // none of the three match. The chain still runs, but nothing Intel-only
        // is asked for.
        let dir = tempfile::tempdir().unwrap();
        stub_vaapi_ffmpeg(dir.path(), "VA-API NVDEC driver");
        let p = planner_over_caps(
            Arc::new(FakeMediaSources {
                sources: vec![source(
                    "abc",
                    vec![nvdec_decodable_video_stream(), audio_stream("aac")],
                )],
                live_streams: HashMap::new(),
            }),
            false,
            vaapi_caps(),
            vaapi_options(),
        );
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(!args.contains("driver=iHD"), "{args}");
        assert!(!args.contains("-low_power"), "{args}");
        // ...but the VAAPI pipeline itself still runs.
        assert!(args.contains("-c:v h264_vaapi"), "{args}");
        assert!(args.contains("scale_vaapi="), "{args}");
    }

    /// A QSV-capable ffmpeg on `platform`.
    fn qsv_caps(
        platform: ferrofin_mediaencoding::encoding_helper::hw::Platform,
    ) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(platform)
            .encoders(["h264_qsv", "hevc_qsv", "libx264"])
            .hwaccels(["qsv", "vaapi", "d3d11va", "opencl", "drm"])
            .filters(ferrofin_mediaencoding::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            // Inside the i915 hang range (5.18 - 6.1.3), which the workaround
            // below depends on.
            .os_version(ferrofin_mediaencoding::encoder::FfmpegVersion::new(6, 1))
            .ffmpeg_version(ferrofin_mediaencoding::encoder::FfmpegVersion::with_build(
                7, 0, 1,
            ))
            .build()
    }

    async fn qsv_plan(
        platform: ferrofin_mediaencoding::encoding_helper::hw::Platform,
        tonemap: bool,
    ) -> String {
        let mut video = nvdec_decodable_video_stream();
        if tonemap {
            video.pixel_format = Some("yuv420p10le".to_owned());
            video.bit_depth = Some(10);
            video.video_range = Some(ferrofin_model::data::VideoRange::Hdr);
            video.color_transfer = Some("smpte2084".to_owned());
            video.codec = Some("hevc".to_owned());
        }
        let p = planner_over_caps(
            Arc::new(FakeMediaSources {
                sources: vec![source("abc", vec![video, audio_stream("aac")])],
                live_streams: HashMap::new(),
            }),
            false,
            qsv_caps(platform),
            EncodingOptions {
                enable_hardware_encoding: true,
                hardware_acceleration_type: HardwareAccelerationType::qsv,
                // See the note in `vaapi_options`: a real render node would
                // make this assertion depend on the test machine having a GPU.
                qsv_device: Some("/dev/null".to_owned()),
                enable_tonemapping: tonemap,
                enable_intel_low_power_h264_hw_encoder: true,
                hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
                ..EncodingOptions::default()
            },
        );
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.max_width = Some(1280);
        req.video_bitrate = Some(3_000_000);
        p.plan(&req, false, None, PlaylistKind::Vod)
            .await
            .unwrap()
            .arguments
            .join(" ")
    }

    #[tokio::test]
    async fn linux_qsv_derives_its_device_from_the_vaapi_one() {
        use ferrofin_mediaencoding::encoding_helper::hw::Platform;
        let args = qsv_plan(Platform::Linux, false).await;
        // QSV is a layer over VAAPI here, and the device graph says so — the
        // configured render node reaches the VAAPI device, and QSV derives.
        assert!(
            args.contains(
                "-init_hw_device vaapi=va:/dev/null,driver=iHD -init_hw_device qsv=qs@va -filter_hw_device qs"
            ),
            "{args}"
        );
        assert!(
            args.contains("scale_vaapi=w=1280:h=720:format=nv12:extra_hw_frames=24"),
            "{args}"
        );
        assert!(
            args.contains("hwmap=derive_device=qsv,format=qsv"),
            "{args}"
        );
        assert!(args.contains("-c:v h264_qsv -low_power 1"), "{args}");
        // The QSV rate-control shape, VBR via maxrate = bitrate + 1.
        assert!(
            args.contains(
                "-mbbrc 1 -b:v 3000000 -maxrate 3000001 -rc_init_occupancy 6000000 -bufsize 12000000"
            ),
            "{args}"
        );
    }

    #[tokio::test]
    async fn the_i915_workaround_reaches_the_command_line_at_last() {
        use ferrofin_mediaencoding::encoding_helper::hw::Platform;
        // Ported in phase 4c but unreachable until QSV was wired: the hang
        // needs an Intel decode feeding an OpenCL tonemap on kernel 5.18-6.1.3.
        let args = qsv_plan(Platform::Linux, true).await;
        assert!(args.contains("-async_depth 1"), "{args}");
        assert!(args.contains("tonemap_opencl="), "{args}");
        assert!(
            args.contains("hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16"),
            "{args}"
        );

        // Without the tonemap there is nothing to work around.
        let plain = qsv_plan(Platform::Linux, false).await;
        assert!(!plain.contains("-async_depth"), "{plain}");
    }

    #[tokio::test]
    async fn windows_qsv_sits_on_d3d11_instead() {
        use ferrofin_mediaencoding::encoding_helper::hw::Platform;
        let args = qsv_plan(Platform::Windows, false).await;
        assert!(
            args.contains(
                "-init_hw_device d3d11va=dx11:,vendor=0x8086 -init_hw_device qsv=qs@dx11 -filter_hw_device qs"
            ),
            "{args}"
        );
        // The relay into QSV, and the pool-forcing option d3d11va needs.
        assert!(
            args.contains("hwmap=derive_device=qsv,vpp_qsv=w=1280:h=720:format=nv12:passthrough=0"),
            "{args}"
        );
        // No VAAPI anywhere on this platform.
        assert!(!args.contains("vaapi"), "{args}");
    }

    #[tokio::test]
    async fn a_dolby_vision_copy_strips_the_metadata_the_client_cannot_play() {
        // The whole point of the bitstream filters: this file copies through to
        // an HDR10 client instead of being re-encoded.
        let mut video = video_stream("hevc");
        video.video_range = Some(ferrofin_model::data::VideoRange::Hdr);
        video.video_range_type = Some(ferrofin_model::data::VideoRangeType::DoviInvalid);
        video.nal_length_size = Some("4".to_owned());
        let p = planner(vec![source("abc", vec![video, audio_stream("aac")])]);
        let mut req = request("abc");
        req.video_codec = Some("copy".to_owned());
        // The client declares its range support per codec in the query string,
        // which is how a real player says "I can play DOVI".
        req.query_string = "hevc-rangetype=DOVI".to_owned();
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-c:v copy"), "{args}");
        // The generic stripper, because this ffmpeg was probed without
        // `hevc_metadata=remove_dovi`. Both spellings are pinned in
        // `bitstream.rs`; what matters here is that the removal reaches the
        // command line at all, appended to the same `-bsf:v` as a filter chain.
        assert!(
            args.contains("-bsf:v hevc_mp4toannexb,dovi_rpu=strip=1"),
            "{args}"
        );
    }

    #[tokio::test]
    async fn a_stream_already_in_annex_b_gets_no_bitstream_filter() {
        // Upstream's `NalLengthSize == "0"` gate, which suppresses the metadata
        // removal as well as the container fixup — so the Dolby Vision RPU
        // survives here even though the client asked for it gone.
        let mut video = video_stream("hevc");
        video.video_range = Some(ferrofin_model::data::VideoRange::Hdr);
        video.video_range_type = Some(ferrofin_model::data::VideoRangeType::DoviInvalid);
        video.nal_length_size = Some("0".to_owned());
        let p = planner(vec![source("abc", vec![video, audio_stream("aac")])]);
        let mut req = request("abc");
        req.video_codec = Some("copy".to_owned());
        // The client declares its range support per codec in the query string,
        // which is how a real player says "I can play DOVI".
        req.query_string = "hevc-rangetype=DOVI".to_owned();
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(!plan.arguments.join(" ").contains("-bsf:v"));
    }

    #[tokio::test]
    async fn a_re_encode_never_gets_a_bitstream_filter() {
        // A fresh bitstream needs neither the fixup nor the strip.
        let mut video = nvdec_decodable_video_stream();
        video.codec = Some("hevc".to_owned());
        video.video_range = Some(ferrofin_model::data::VideoRange::Hdr);
        video.video_range_type = Some(ferrofin_model::data::VideoRangeType::DoviInvalid);
        video.nal_length_size = Some("4".to_owned());
        let p = planner(vec![source("abc", vec![video, audio_stream("aac")])]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        // The client declares its range support per codec in the query string,
        // which is how a real player says "I can play DOVI".
        req.query_string = "hevc-rangetype=DOVI".to_owned();
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(!plan.arguments.join(" ").contains("-bsf:v"));
    }

    #[tokio::test]
    async fn a_graphical_subtitle_maps_the_graph_output_not_the_video() {
        // The `[v]` pad belongs to Ferrofin's own software overlay graph. The
        // ported chains leave their output unlabeled for ffmpeg to pick up, so
        // naming `[v]` here would abort with "Output with label 'v' does not
        // exist in any defined filter graph".
        let p = nvenc_planner(vec![source(
            "abc",
            vec![
                nvdec_decodable_video_stream(),
                audio_stream("aac"),
                subtitle_stream("pgssub", 2),
            ],
        )]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.subtitle_stream_index = Some(2);
        req.subtitle_method = Some("Encode".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(!args.contains("-map [v]"), "{args}");
        assert!(args.contains("-map 0:0 -map 0:1 -map -0:0"), "{args}");
        // The bitmap is pre-processed and uploaded, then composited on the GPU.
        assert!(args.contains("[0:2]scale,scale="), "{args}");
        assert!(args.contains("hwupload=derive_device=cuda[sub]"), "{args}");
        assert!(
            args.contains("overlay_cuda=eof_action=pass:repeatlast=0"),
            "{args}"
        );
        // ...and NOT with the premultiplied option, which is text-only.
        assert!(!args.contains("alpha_format"), "{args}");
    }

    #[tokio::test]
    async fn an_exact_requested_size_reaches_the_hardware_scaler() {
        // `Width`/`Height` are a different request from `MaxWidth`/`MaxHeight`:
        // one pins the output, the other bounds it. Dropping the exact pair
        // silently gives the client a different resolution than it asked for.
        let p = nvenc_planner(vec![source(
            "abc",
            vec![nvdec_decodable_video_stream(), audio_stream("aac")],
        )]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.width = Some(1280);
        req.height = Some(720);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("scale_cuda=w=1280:h=720"), "{args}");
    }

    #[tokio::test]
    async fn a_seek_shifts_the_subtitle_clock_on_the_hardware_path_too() {
        // The one that bit: a per-segment `-ss` restarts decoded frames at
        // PTS 0 (Ferrofin passes no `-copyts`), so the generated subtitle
        // source must start at zero and the `subtitles` filter be shifted
        // around instead. Starting the source at the seek would put it in a
        // time range the video never reaches and nothing would be drawn.
        // Verified against real ffmpeg: with the shift the burned frame is
        // byte-identical to the same moment rendered without a seek; without
        // it, it is not.
        let p = nvenc_planner(vec![source(
            "abc",
            vec![
                nvdec_decodable_video_stream(),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        )]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.subtitle_stream_index = Some(2);
        req.subtitle_method = Some("Encode".to_owned());
        let plan = p
            .plan(&req, false, Some(10), PlaylistKind::Vod)
            .await
            .unwrap();
        let args = plan.arguments.join(" ");
        // Segment 10 of 3 s segments = 30 s in.
        assert!(args.contains("setpts=PTS+30/TB,subtitles="), "{args}");
        assert!(args.contains(",setpts=PTS-30/TB,hwupload"), "{args}");
        assert!(
            args.contains("alphasrc=s=1920x1080:r=10:start='0'"),
            "{args}"
        );

        // Segment 0 needs no shift at all.
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert!(!plan.arguments.join(" ").contains("setpts"));
    }

    #[tokio::test]
    async fn an_interlaced_source_deinterlaces_on_the_gpu() {
        let mut video = nvdec_decodable_video_stream();
        video.is_interlaced = true;
        let p = nvenc_planner(vec![source("abc", vec![video, audio_stream("aac")])]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("yadif_cuda="), "{args}");
    }

    #[tokio::test]
    async fn an_hdr_source_tonemaps_on_the_gpu() {
        let mut video = hdr_4k_video_stream("hevc");
        video.pixel_format = Some("yuv420p10le".to_owned());
        video.video_range = Some(ferrofin_model::data::VideoRange::Hdr);
        // Both switches are off by default and both are load-bearing here:
        // HEVC is not in the default hardware-decode list, and tonemapping is
        // opt-in.
        let p = planner_over_caps(
            Arc::new(FakeMediaSources {
                sources: vec![source("abc", vec![video, audio_stream("aac")])],
                live_streams: HashMap::new(),
            }),
            false,
            nvenc_caps_full(),
            EncodingOptions {
                enable_hardware_encoding: true,
                hardware_acceleration_type: HardwareAccelerationType::nvenc,
                encoder_preset: EncoderPreset::medium,
                enable_tonemapping: true,
                hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
                ..EncodingOptions::default()
            },
        );
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("tonemap_cuda="), "{args}");
        // The colour properties describe the HDR source entering the graph.
        assert!(
            args.contains("setparams=color_primaries=bt2020:color_trc=smpte2084"),
            "{args}"
        );
    }

    #[tokio::test]
    async fn a_burned_in_subtitle_becomes_a_cuda_overlay() {
        // Verified live against ffmpeg n9.0.1 + an RTX 5090 in the shape below,
        // modulo `alphasrc`, which is a jellyfin-ffmpeg filter that stock
        // ffmpeg does not carry (and which the gate above declines without).
        let p = nvenc_planner(vec![source(
            "abc",
            vec![
                nvdec_decodable_video_stream(),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        )]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        req.max_width = Some(1280);
        req.subtitle_stream_index = Some(2);
        req.subtitle_method = Some("Encode".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");

        // The raw video is mapped and then cancelled: ffmpeg adds the graph's
        // unlabeled output by itself, so without the negative map the output
        // carries two video streams — verified against real ffmpeg, as was the
        // ordering (a leading negative map is an error).
        assert!(args.contains("-map 0:0 -map 0:1 -map -0:0"), "{args}");
        // ...and no `[v]` pad, which the ported graph never defines.
        assert!(!args.contains("-map [v]"), "{args}");

        // The text is drawn onto a generated transparent source sized to the
        // OUTPUT, uploaded, and composited by the CUDA overlay — the frames
        // never come back to system memory.
        let graph = args
            .split("-filter_complex ")
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            graph.split(" -c:v").next().unwrap(),
            "alphasrc=s=1280x720:r=10:start='0',format=yuva420p,\
             subtitles=f='/media/movie.mkv':si=0:alpha=1:sub2video=1,\
             hwupload=derive_device=cuda[sub];\
             [0:0]setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709,\
             scale_cuda=w=1280:h=720:format=yuv420p[main];\
             [main][sub]overlay_cuda=eof_action=pass:repeatlast=0:\
             alpha_format=premultiplied"
        );
        assert!(!args.contains("hwdownload"), "{args}");
        // The old bolt-on emitted a separate `-vf` here; the two cannot coexist.
        assert!(!args.contains("-vf "), "{args}");
    }

    #[tokio::test]
    async fn nvenc_falls_back_when_ffmpeg_lacks_the_encoder() {
        // Selection is a probe result, not a table: an ffmpeg built without
        // `av1_nvenc` must not be handed one just because NVENC is configured.
        let src = source(
            "abc",
            vec![nvdec_decodable_video_stream(), audio_stream("aac")],
        );
        // A *complete* CUDA build that simply has no NVENC encoder — the
        // interesting case. Building caps with no hwaccel at all would make
        // this pass for the wrong reason, since nothing hardware would be
        // emitted regardless of the encoder.
        let caps = FfmpegCapabilities::builder()
            .platform(ferrofin_mediaencoding::encoding_helper::hw::Platform::Linux)
            .encoders(["libx264"])
            .hwaccels(["cuda"])
            .filters(ferrofin_mediaencoding::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(ferrofin_mediaencoding::encoder::FfmpegVersion::with_build(
                7, 0, 1,
            ))
            .build();
        let p = planner_over_caps(
            Arc::new(FakeMediaSources {
                sources: vec![src],
                live_streams: HashMap::new(),
            }),
            false,
            caps,
            EncodingOptions {
                enable_hardware_encoding: true,
                hardware_acceleration_type: HardwareAccelerationType::nvenc,
                ..EncodingOptions::default()
            },
        );
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.allow_video_stream_copy = false;
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-c:v libx264"), "{args}");
        // Hardware DECODE still happens — it is selected for the accelerator,
        // not the encoder — so the graph must bring the frames back down rather
        // than hand GPU surfaces to a software encoder, which ffmpeg rejects
        // with "Impossible to convert between the formats supported by the
        // filter ... and auto_scale_0".
        assert!(args.contains("-hwaccel cuda"), "{args}");
        assert!(args.contains("hwdownload,format=yuv420p"), "{args}");
    }

    /// The parity fixture (320x240 h264 @ ~6 Mbps container bitrate, mono aac)
    /// under the harness query. `GetStreamingState` yields
    /// `OutputAudioBitrate = min(1 × 128000, 128000)` and `OutputVideoBitrate =
    /// min(ScaleBitrate(GetMinBitrate(6M, 1M)), 1M) = 1_000_000` — the
    /// 1_128_000 BANDWIDTH Jellyfin advertises. A remux carries the video
    /// bitrate too (it only gates the bitrate *args*), and the audio bitrate is
    /// computed against the requested codec even when the audio is copied.
    #[tokio::test]
    async fn plan_output_bitrates_follow_get_streaming_state() {
        let mut video = video_stream("h264");
        video.width = Some(320);
        video.height = Some(240);
        video.bit_rate = Some(6_000_000);
        let mut audio = audio_stream("aac");
        audio.channels = Some(1);
        let src = source("abc", vec![video, audio]);

        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.audio_codec = Some("aac".to_owned());
        req.audio_bitrate = Some(128_000);
        req.video_bitrate = Some(1_000_000);
        req.max_width = Some(320);
        req.transcoding_max_audio_channels = Some(2);
        req.allow_video_stream_copy = false;
        req.allow_audio_stream_copy = false;
        let p = planner(vec![src.clone()]);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.state.output_audio_channels, Some(1));
        assert_eq!(plan.state.output_audio_bitrate, Some(128_000));
        assert_eq!(plan.state.output_video_bitrate, Some(1_000_000));
        assert_eq!(plan.state.output_video_codec.as_deref(), Some("h264"));
        let args = plan.arguments.join(" ");
        assert!(args.contains("-maxrate 1000000"), "re-encode caps: {args}");
        assert!(args.contains("-b:a 128000"), "audio bitrate: {args}");

        // A copy-eligible request (8 Mbps cap above the 6 Mbps source): both
        // streams copy, the bitrates are still the GetStreamingState values
        // (video: min(ScaleBitrate(GetMinBitrate(6M, 8M)), 8M) = 6M), and no
        // bitrate args are emitted.
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        req.audio_codec = Some("aac".to_owned());
        req.audio_bitrate = Some(128_000);
        req.video_bitrate = Some(8_000_000);
        req.max_width = Some(320);
        let p = planner(vec![src]);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.state.output_video_codec.as_deref(), Some("copy"));
        assert_eq!(plan.state.output_audio_codec.as_deref(), Some("copy"));
        assert_eq!(plan.state.output_video_bitrate, Some(6_000_000));
        assert_eq!(plan.state.output_audio_bitrate, Some(128_000));
        let args = plan.arguments.join(" ");
        assert!(
            !args.contains("-maxrate"),
            "copy has no bitrate args: {args}"
        );
        assert!(!args.contains("-b:a"), "copy has no audio bitrate: {args}");
        assert_eq!(
            plan.min_segments, 3,
            "3s segments → 3 (StreamState.MinSegments)"
        );
    }

    /// A lossless target reports the source's own bitrate; an absent request
    /// bitrate still yields the per-channel default (`GetAudioBitrateParam`
    /// never returns null with an audio stream).
    #[tokio::test]
    async fn plan_audio_bitrate_defaults_and_lossless() {
        let mut audio = audio_stream("dts");
        audio.channels = Some(6);
        audio.bit_rate = Some(1_536_000);
        let src = source("abc", vec![video_stream("h264"), audio]);
        let p = planner(vec![src.clone()]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        // 6 in, 6 out → min(640000, MAX).
        assert_eq!(plan.state.output_audio_bitrate, Some(640_000));

        let p = planner(vec![src]);
        let mut req = request("abc");
        req.audio_codec = Some("flac".to_owned());
        req.audio_bitrate = Some(128_000);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.state.output_audio_bitrate, Some(1_536_000));
        assert_eq!(plan.min_segments, 3);
        // An explicit MinSegments wins; 10s+ segments default to 2.
        let p = planner(vec![source("abc", vec![video_stream("h264")])]);
        let mut req = request("abc");
        req.segment_length = Some(10);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.min_segments, 2);
        req.min_segments = Some(1);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.min_segments, 1);
    }

    /// `ParseStreamOptions`: lower-case-initial query keys reach the
    /// per-codec option lookups (`h264-profile`/`h264-level`/`aac-profile`);
    /// the typed `Profile`/`Level`/`Framerate`/`Width`/`Height` land on the
    /// base request.
    #[tokio::test]
    async fn plan_parses_lowercase_stream_options_and_typed_fields() {
        let src = source("abc", vec![video_stream("hevc"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.query_string =
            "?MediaSourceId=abc&h264-profile=high&h264-level=51&aac-profile=HE&VideoCodec=h264"
                .to_owned();
        req.video_codec = Some("h264".to_owned());
        req.framerate = Some(24.0);
        req.width = Some(640);
        req.height = Some(360);
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(
            plan.state.requested_profiles("h264"),
            vec!["high".to_owned()]
        );
        assert_eq!(plan.state.requested_level("h264").as_deref(), Some("51"));
        assert_eq!(plan.state.requested_profiles("aac"), vec!["HE".to_owned()]);
        // …and they reach the encoder args, as jellyfin-web's TranscodingUrl
        // (`h264-profile=high&h264-level=51`) does on Jellyfin.
        let args = plan.arguments.join(" ");
        assert!(args.contains("-profile:v:0 high"), "{args}");
        assert!(args.contains("-level 51"), "{args}");
        // PascalCase keys are NOT stream options.
        assert!(
            !plan
                .state
                .base_request
                .stream_options
                .iter()
                .any(|(k, _)| k == "MediaSourceId" || k == "VideoCodec"),
            "{:?}",
            plan.state.base_request.stream_options
        );
        assert_eq!(plan.state.base_request.framerate, Some(24.0));
        assert_eq!(plan.state.base_request.width, Some(640));
        assert_eq!(plan.state.base_request.height, Some(360));

        // The typed `Profile`/`Level` take precedence over the per-codec option.
        let src = source("abc", vec![video_stream("hevc"), audio_stream("aac")]);
        let p = planner(vec![src]);
        let mut req = request("abc");
        req.query_string = "?h264-profile=high&h264-level=51".to_owned();
        req.profile = Some("main".to_owned());
        req.level = Some("40".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(
            plan.state.requested_profiles("h264"),
            vec!["main".to_owned()]
        );
        assert_eq!(plan.state.requested_level("h264").as_deref(), Some("40"));
    }

    /// `TryStreamCopy` runs only for video requests: an audio-only HLS request
    /// with a matching source codec still re-encodes (the variant URL keeps
    /// `audioCodec=aac`, as Jellyfin's does).
    #[tokio::test]
    async fn plan_audio_request_never_stream_copies() {
        let src = source("abc", vec![audio_stream("aac")]);
        let p = planner(vec![src.clone()]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        let plan = p.plan(&req, true, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.state.output_audio_codec.as_deref(), Some("aac"));
        assert!(
            !plan.arguments.join(" ").contains("-c:a copy"),
            "{:?}",
            plan.arguments
        );

        // The same source on the video route copies the matching audio.
        let p = planner(vec![source(
            "abc",
            vec![video_stream("h264"), audio_stream("aac")],
        )]);
        let mut req = request("abc");
        req.audio_codec = Some("aac".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(plan.state.output_audio_codec.as_deref(), Some("copy"));
    }

    /// An EVENT plan (`live.m3u8`) writes an event playlist with routed
    /// segment URIs, the `superfast` preset and, for mpegts, no global header;
    /// a VOD plan keeps `vod`/`veryfast` and neither flag.
    #[tokio::test]
    async fn plan_event_playlist_args_follow_get_live_hls_stream() {
        let src = source("abc", vec![video_stream("hevc"), audio_stream("aac")]);
        let p = planner(vec![src.clone()]);
        let mut req = request("abc");
        req.video_codec = Some("h264".to_owned());
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Event)
            .await
            .unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-hls_playlist_type event"), "{args}");
        let stem = plan.playlist_path.file_stem().unwrap().to_string_lossy();
        assert!(
            args.contains(&format!("-hls_base_url hls/{stem}/")),
            "{args}"
        );
        assert!(args.contains("-flags -global_header"), "{args}");
        assert!(args.contains("-preset superfast"), "{args}");
        assert!(!args.contains("-preset veryfast"), "{args}");

        let p = planner(vec![src.clone()]);
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Vod)
            .await
            .unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-hls_playlist_type vod"), "{args}");
        assert!(!args.contains("-hls_base_url"), "{args}");
        assert!(!args.contains("-global_header"), "{args}");
        assert!(args.contains("-preset veryfast"), "{args}");

        // fMP4 event segments keep the global header (the flag is mpegts-only).
        let p = planner(vec![src]);
        req.segment_container = Some("mp4".to_owned());
        let plan = p
            .plan(&req, false, Some(0), PlaylistKind::Event)
            .await
            .unwrap();
        let args = plan.arguments.join(" ");
        assert!(args.contains("-hls_playlist_type event"), "{args}");
        assert!(!args.contains("-global_header"), "{args}");
    }

    /// No selected subtitle: the delivery method is the DTO default
    /// (`External`), never `Hls` — the master playlist keys its subtitle group
    /// off it. The typed request fields select and deliver like the query.
    #[tokio::test]
    async fn plan_subtitle_method_defaults_to_external_without_a_selection() {
        let src = source(
            "abc",
            vec![
                video_stream("h264"),
                audio_stream("aac"),
                subtitle_stream("subrip", 2),
            ],
        );
        let p = planner(vec![src.clone()]);
        let plan = p
            .plan(&request("abc"), false, None, PlaylistKind::Vod)
            .await
            .unwrap();
        assert_eq!(
            plan.state.subtitle_delivery_method,
            SubtitleDeliveryMethod::External
        );
        assert!(plan.state.subtitle_stream.is_none());

        let p = planner(vec![src]);
        let mut req = request("abc");
        req.subtitle_stream_index = Some(2);
        req.subtitle_method = Some("Hls".to_owned());
        let plan = p.plan(&req, false, None, PlaylistKind::Vod).await.unwrap();
        assert_eq!(
            plan.state.subtitle_delivery_method,
            SubtitleDeliveryMethod::Hls
        );
        assert_eq!(
            plan.state.subtitle_stream.as_ref().map(|s| s.index),
            Some(2)
        );
    }
}
