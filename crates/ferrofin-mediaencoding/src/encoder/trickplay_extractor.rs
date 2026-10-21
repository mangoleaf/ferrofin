//! [`TrickplayFrameExtractorImpl`] — the concrete
//! [`TrickplayFrameExtractor`] over the [`Transcoder`] process seam.
//!
//! Port of the software path of
//! `MediaEncoder.ExtractVideoImagesOnIntervalAccelerated` /
//! `ExtractVideoImagesOnIntervalInternal`: a single ffmpeg run with an
//! `fps=1/interval` sampling filter plus the width-bounded, DAR-preserving
//! `scale` expression, `-c:v mjpeg -qscale:v …`, writing `%08d.jpg` frames into
//! a caller-supplied directory.
//!
//! The **hardware** path is [`build_accelerated_trickplay_args`], a port of the
//! rest of the same C# function: it runs the synthetic MJPEG job through the
//! hardware matrix in [`crate::encoding_helper::hw`]. It is chosen per job when
//! the request allows hardware, the configured accelerator has a ported filter
//! chain, and a video stream is available to size it from — otherwise the
//! software path, which is also where a failed keyframe-only run retries.
//!
//! The three switches come from `TrickplayOptions`, **not** from the playback
//! encoding options: an operator can run playback on a GPU and trickplay on the
//! CPU, or the reverse. Reading the wrong one is how a setting silently stops
//! meaning anything, so the request carries them explicitly.
//!
//! Departures from the C# (documented per the port rules):
//! - The `fps=` filter is emitted unconditionally. Upstream routes the
//!   interval through `GetFramerateParam`, which returns nothing unless the
//!   stream carries a `ReferenceFrameRate` *greater* than the requested rate,
//!   so a stream ffprobe could not read a frame rate for gets no `fps=` filter
//!   at all. Upstream then extracts a thumbnail from **every keyframe** in
//!   keyframe-only mode, filling a disk with JPEGs, and with keyframe-only off
//!   it throws "EncodingHelper returned invalid filter parameters" (12.0-rc3,
//!   PR 17304), so the item gets no trickplay. Ferrofin keeps the sampling
//!   filter in both modes (the "do not port Jellyfin bugs" rule). Without a
//!   frame rate there is nothing to rebuild timestamps at, so such a stream is
//!   sampled on its container timestamps with no `setpts` in front: upstream's
//!   `30` fallback for that rebuild is never reached there. The two agree on
//!   every stream that has a frame rate, which is all of them in practice.
//! - The C# creates the temp output directory itself; here the caller owns the
//!   output directory (the trickplay manager passes a temp dir and cleans it
//!   up), so the extractor only creates and fills it.

use std::path::Path;
use std::sync::Arc;

use crate::encoder::FfmpegVersion;
use crate::encoder::encoding_utils::quote;
use crate::encoding_helper::hw::FfmpegCapabilities;
use crate::error::MediaEncodingError;
use async_trait::async_trait;
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::HardwareAccelerationType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayFrameExtractor};

use super::Transcoder;

/// The input frame rate `setpts` rebuilds a full decode's timestamps at: the
/// stream's `ReferenceFrameRate` when it is positive (`MediaEncoder.cs:945`).
///
/// `None` without one. Upstream falls back to `30` there, but only after
/// `GetFramerateParam` has already left such a stream without the `fps=`
/// sampler the rebuild is spliced in front of, so upstream throws first and
/// the fallback never runs (see the module doc).
fn pts_frame_rate(stream: Option<&MediaStream>) -> Option<f32> {
    stream
        .and_then(MediaStream::reference_frame_rate)
        .filter(|rate| *rate > 0.0)
}

/// `setpts=N/{rate:F3}/TB,`, the filter upstream splices in front of the
/// `fps=` sampler (`MediaEncoder.cs:948`). Numbering frames from the decode
/// counter stops a container with corrupt timestamps or duration from making
/// the sampler emit frames without bound.
fn pts_normalizer(rate: f32) -> String {
    format!("setpts=N/{rate:.3}/TB,")
}

/// ` -map 0:{index}`, which upstream appends to the trickplay input argument
/// (`MediaEncoder.cs:925-932`): "Force the video stream, otherwise ffmpeg may
/// pick a cover image." Empty when no video stream was handed over.
fn map_argument(map_index: Option<i32>) -> String {
    map_index
        .map(|index| format!(" -map 0:{index}"))
        .unwrap_or_default()
}

/// The concrete trickplay frame extractor: builds the ffmpeg argument line and
/// runs it through the [`Transcoder`] seam (a real spawn in production, a fake
/// in unit tests).
pub struct TrickplayFrameExtractorImpl<T: Transcoder> {
    transcoder: Arc<T>,
    image_encoding_pool: Arc<tokio::sync::Semaphore>,
    ffmpeg_path: String,
    ffmpeg_version: Option<FfmpegVersion>,
    caps: Arc<FfmpegCapabilities>,
    config: Option<Arc<dyn ServerConfigurationManager>>,
}

impl<T: Transcoder> std::fmt::Debug for TrickplayFrameExtractorImpl<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrickplayFrameExtractorImpl")
            .field("ffmpeg_path", &self.ffmpeg_path)
            .field("ffmpeg_version", &self.ffmpeg_version)
            .finish_non_exhaustive()
    }
}

impl<T: Transcoder> TrickplayFrameExtractorImpl<T> {
    /// Shares the startup thumbnail/trickplay ceiling with the other extractor.
    /// The host resolves the dashboard setting and supplies a nonzero pool.
    #[must_use]
    pub fn with_image_encoding_pool(mut self, pool: Arc<tokio::sync::Semaphore>) -> Self {
        self.image_encoding_pool = pool;
        self
    }

    /// Creates a **software-only** extractor spawning `ffmpeg_path` through
    /// `transcoder`.
    ///
    /// `ffmpeg_version` is the probed version, which decides `-fps_mode` versus
    /// the deprecated `-vsync`; `None` means "unprobed" and takes the
    /// conservative `-vsync`, which every supported build still understands.
    ///
    /// Without capabilities and a configuration handle there is nothing to
    /// build a hardware pipeline from, so every job takes the software path.
    /// Use [`Self::with_hardware`] to enable the accelerated one.
    pub fn new(
        transcoder: Arc<T>,
        ffmpeg_path: impl Into<String>,
        ffmpeg_version: Option<FfmpegVersion>,
    ) -> Self {
        Self {
            transcoder,
            image_encoding_pool: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            ffmpeg_path: ffmpeg_path.into(),
            ffmpeg_version,
            caps: Arc::new(FfmpegCapabilities::default()),
            config: None,
        }
    }

    /// Enables the hardware path, using `caps` (the startup probe) and reading
    /// [`EncodingOptions`] from `config` on **every** job.
    ///
    /// Read per job, not captured: the encoding options are runtime-mutable
    /// from the dashboard, so an operator who switches accelerator mid-scan
    /// must not have the old choice baked into this extractor. Upstream reads
    /// them the same way, inside `ExtractVideoImagesOnIntervalAccelerated`.
    #[must_use]
    pub fn with_hardware(
        mut self,
        caps: Arc<FfmpegCapabilities>,
        config: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        // The probed capabilities carry the ffmpeg version, so adopt it and
        // keep one source. Otherwise a hardware run and its software retry can
        // disagree about `-fps_mode` versus `-vsync` inside the same job.
        if let Some(version) = caps.ffmpeg_version() {
            self.ffmpeg_version = Some(version);
        }
        self.caps = caps;
        self.config = Some(config);
        self
    }

    /// The encoding options to plan against, or `None` when this extractor was
    /// built without a configuration handle (software only).
    async fn encoding_options(&self) -> Option<EncodingOptions> {
        let config = self.config.as_ref()?;
        match config.get_encoding_options().await {
            Ok(options) => Some(options),
            Err(e) => {
                // Debug, not warn: this runs once per (item x width), so a
                // malformed `encoding.json` would otherwise emit one warning
                // per job for a whole library pass. LOGGING.md forbids a level
                // above `debug` whose volume scales with library size.
                //
                // Failing the job would be worse than degrading: trickplay
                // must not stop because a config file cannot be parsed, and
                // the software path needs none of it.
                tracing::debug!("cannot read encoding options for trickplay: {e}");
                None
            }
        }
    }
}

/// Builds the ffmpeg argument line for one trickplay frame-extraction run.
///
/// Mirrors the software branch of the C# `ExtractVideoImagesOnIntervalInternal`
/// format string: `-loglevel error {input} -an -sn {filter} -threads {t}
/// -c:v mjpeg -qscale:v {q}{sync} -f image2 "{out}"`, where `{sync}` is the
/// version-gated [`video_sync_option`]. The `fps` rate is the
/// exact rational `1000/interval_ms`; the `scale` expression is Jellyfin's
/// width-bounded software scaler (`trunc(min(max(iw,ih*dar),W)/2)*2` by
/// `trunc(ow/dar/2)*2`), which keeps both dimensions even and honours
/// anamorphic sources.
///
/// `pts_frame_rate` is `Some` when the timestamps are rebuilt in front of the
/// sampler (keyframe-only extraction configured off; see [`pts_normalizer`]).
/// `map_index` is the video stream's ffmpeg index, mapped right after the
/// input (` -map 0:N`, [`map_argument`]).
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "the flat argument list of upstream's format string; the same \
              shape as `extract_image_arguments` next door"
)]
pub fn build_trickplay_args(
    input_path: &str,
    interval_ms: i32,
    max_width: i32,
    qscale: i32,
    threads: i32,
    output_pattern: &str,
    ffmpeg_version: Option<FfmpegVersion>,
    keyframe_only: bool,
    pts_frame_rate: Option<f32>,
    map_index: Option<i32>,
) -> String {
    // ffmpeg qscale is 1 (best) – 31 (worst); C# clamps the configured value.
    let qscale = qscale.clamp(1, 31);
    // Upstream funnels the software and hardware paths through the same
    // `ExtractVideoImagesOnIntervalInternal`, so this gets the same
    // version-gated `-fps_mode`/`-vsync` choice the hardware path does.
    let sync = crate::encoding_helper::helper::video_sync_option("0", ffmpeg_version);
    // Upstream prepends this to the input fragment *regardless* of whether
    // hardware is in play, so a job that fell back to software because its
    // accelerator cannot skip to keyframes still skips to keyframes on the CPU
    // -- which is the whole reason falling back is acceptable.
    let skip_frame = if keyframe_only {
        "-skip_frame nokey "
    } else {
        ""
    };
    let setpts = pts_frame_rate.map(pts_normalizer).unwrap_or_default();
    let map = map_argument(map_index);
    format!(
        "-loglevel error {skip_frame}-threads {threads} -i file:{input}{map} -an -sn \
         -vf \"{setpts}fps=1000/{interval_ms},scale=trunc(min(max(iw\\,ih*dar)\\,{max_width})/2)*2:trunc(ow/dar/2)*2\" \
         -threads {threads} -c:v mjpeg -qscale:v {qscale}{sync} -f image2 {output}",
        input = quote(input_path),
        output = quote(output_pattern),
    )
}

/// One trickplay extraction run, as the argument builders read it.
#[derive(Debug, Clone, Copy)]
pub struct TrickplayJob<'a> {
    /// The source media path.
    pub input_path: &'a str,
    /// The source video stream, which the hardware chain sizes itself from.
    pub stream: &'a MediaStream,
    /// Milliseconds between thumbnails.
    pub interval_ms: i32,
    /// The thumbnail width bound in pixels.
    pub max_width: i32,
    /// ffmpeg `-qscale:v`, 1 (best) to 31 (worst).
    pub qscale: i32,
    /// The ffmpeg thread count; `0` lets ffmpeg decide.
    pub threads: i32,
    /// The numbered-JPEG output pattern.
    pub output_pattern: &'a str,
    /// `TrickplayOptions.EnableHwAcceleration` — the trickplay-specific
    /// hardware switch, separate from the global encoding options. Off means
    /// this job takes the software path no matter what the server is
    /// configured to do for playback.
    pub allow_hw_accel: bool,
    /// `TrickplayOptions.EnableHwEncoding` — whether the MJPEG *encoder* may
    /// be a hardware one. Independent of `allow_hw_accel`: hardware decode
    /// with a software MJPEG encoder is a valid and common configuration.
    pub enable_hw_encoding: bool,
    /// `TrickplayOptions.EnableKeyFrameOnlyExtraction` — decode keyframes
    /// only. This is what makes the hardware path worth taking, and it is also
    /// the thing that can fail on a file with a broken keyframe index: a
    /// caller that gets an ffmpeg failure should rebuild the job with this
    /// `false` and retry, as upstream does.
    pub keyframe_only: bool,
    /// `Some(rate)` when the timestamps are rebuilt in front of the `fps=`
    /// sampler ([`pts_normalizer`]). Decided by the CONFIGURED keyframe-only
    /// flag, not by `keyframe_only`: upstream builds the filter once, so the
    /// standard-way retry of a keyframe-only job carries none.
    pub pts_frame_rate: Option<f32>,
    /// The video stream's ffmpeg input index ([`map_argument`]).
    pub map_index: Option<i32>,
}

/// Builds the ffmpeg argument line and process environment for a **hardware**
/// trickplay run, or `None` when this job should take the software path.
///
/// Port of `MediaEncoder.ExtractVideoImagesOnIntervalAccelerated`. The whole
/// point is that trickplay decodes a long file to produce a handful of frames,
/// which is exactly what a GPU is good at and what makes a library-wide
/// trickplay pass take hours in software.
///
/// Returns `None` — meaning "use [`build_trickplay_args`]" — whenever hardware
/// is switched off for trickplay, no accelerator is configured, the configured
/// accelerator has no ported filter chain (see the `match` below), or
/// keyframe-only extraction was asked for and this accelerator's decoder
/// cannot do it. That last case is upstream's: decoding *every* frame on the
/// GPU is slower than decoding keyframes on the CPU, so it drops to software
/// rather than accept the trade.
///
/// The environment is not decoration: the VAAPI branch sets
/// `LIBVA_DRIVER_NAME`/`LIBVA_DRIVER_NAME_JELLYFIN` and `AMD_DEBUG`, and on a
/// host with more than one libva driver installed, dropping them picks the
/// wrong one. Callers must apply it to the ffmpeg child, as the planner does.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one synthetic encoding job assembled field by field, as upstream \
              does; splitting it would hide which fields the trickplay path \
              sets differently from a real transcode"
)]
pub fn build_accelerated_trickplay_args(
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    job: &TrickplayJob<'_>,
) -> Option<(String, Vec<(String, String)>)> {
    use crate::encoding_helper::hw;

    let &TrickplayJob {
        input_path,
        stream,
        interval_ms,
        max_width,
        qscale,
        threads,
        output_pattern,
        allow_hw_accel,
        enable_hw_encoding,
        keyframe_only,
        pts_frame_rate,
        map_index,
    } = job;

    if !allow_hw_accel || options.hardware_acceleration_type == HardwareAccelerationType::none {
        return None;
    }
    // Upstream consults the keyframe gate *only* when keyframe-only extraction
    // is switched on. With it off, hardware still helps — it just decodes every
    // frame — so gating unconditionally here would silently lose the GPU for
    // anyone who turned keyframe-only off.
    if keyframe_only && !supports_keyframe_only_decode(caps, options) {
        return None;
    }
    // `fps=` would be infinite, and a zero width reaches the scaler as
    // `w=0:h=0`. The trait impl rejects both on the software path; the
    // hardware path has to as well, or it becomes the way round the check.
    if interval_ms <= 0 || max_width <= 0 {
        return None;
    }

    // A synthetic job describing "decode this, emit mjpeg thumbnails". The
    // frame rate is the interval expressed as fps, which is what puts the
    // `fps=` filter at the head of the chain.
    let requested = hw::decoder::RequestedSize {
        width: None,
        height: None,
        max_width: Some(max_width),
        max_height: None,
    };
    let decode_ctx = hw::decoder::DecodeContext {
        caps,
        options,
        video_stream: Some(stream),
        video_type: None,
        output_video_codec: Some("mjpeg"),
        requested,
    };
    // Two switches, both of which must be on: the trickplay-specific one and
    // the global "hardware encoding" checkbox that `mjpeg_encoder` itself
    // honours. An operator who wants hardware *decode* for trickplay but a
    // software MJPEG encoder gets it by turning only the first one off.
    let video_encoder = if enable_hw_encoding {
        hw::encoder::mjpeg_encoder(
            caps,
            options.hardware_acceleration_type,
            None,
            options.enable_hardware_encoding,
        )
    } else {
        hw::encoder::DEFAULT_MJPEG_ENCODER.to_owned()
    };
    let render_node = hw::device_init::RenderNode::resolve(options.vaapi_device.as_deref());
    let qsv_node = hw::device_init::RenderNode::resolve(options.qsv_device.as_deref());
    let hwaccel = hw::input_args::input_video_hwaccel_args(
        &decode_ctx,
        &video_encoder,
        render_node,
        qsv_node,
        true,
    );
    let video_decoder = hw::decoder::hardware_video_decoder(&decode_ctx).unwrap_or_default();

    let chain_input = hw::sw_chain::ChainInput {
        caps,
        options,
        video_encoder: &video_encoder,
        video_decoder: &video_decoder,
        video_width: stream.width,
        // Hardware scalers take fixed pixel sizes, so an anamorphic source has
        // to be un-stretched here rather than left to a `dar` expression.
        video_height: display_corrected_height(stream).or(stream.height),
        requested,
        three_d_format: None,
        rotation: stream.rotation,
        color_transfer: stream.color_transfer.as_deref(),
        reference_frame_rate: stream.reference_frame_rate(),
        real_frame_rate: stream.real_frame_rate,
        start_time_ticks: 0,
        deinterlace: false,
        do_sw_tonemap: false,
        do_hw_tonemap: hw::input_args::is_hw_tonemap_available(&decode_ctx, &video_decoder),
        vulkan_tonemap_available: hw::tonemap::is_vulkan_hw_tonemap_available(
            options,
            Some(stream),
        ),
        vpp_tonemap_available: hw::tonemap::is_intel_vpp_tonemap_available(
            caps,
            options,
            Some(stream),
        ),
        source_codec: stream.codec.as_deref(),
        requires_dovi_reshaping: hw::tonemap::requires_dovi_reshaping(Some(stream)),
        is_hevc_rext: hw::decoder::is_video_stream_hevc_rext(Some(stream)),
        subtitle: hw::sw_chain::SubtitleOverlay::None,
    };
    let chain = match options.hardware_acceleration_type {
        HardwareAccelerationType::vaapi => hw::vaapi::vaapi_vid_filter_chain(&chain_input),
        HardwareAccelerationType::qsv => hw::qsv::intel_vid_filter_chain(&chain_input),
        HardwareAccelerationType::nvenc => hw::nvidia::nvidia_vid_filter_chain(&chain_input),
        // AMF, VideoToolbox, RKMPP and V4L2M2M have no ported filter chain.
        // That is the owner's scope decision (CLAUDE.md, "Current scope"), not
        // an oversight, and it is why `supports_keyframe_only_decode` can say
        // `true` for three of them while this returns `None` — that predicate
        // is a faithful port of upstream's, and the chain is the part that
        // does not exist. Supporting one means porting its chain, its
        // `GetEncoderParam`/`GetVideoBitrateParam` arms, its trickplay extras
        // (`-hwaccel_flags +low_priority` for VideoToolbox, `-allow_sw 1`, and
        // the VideoToolbox/RKMPP arms of `mjpeg_quality`), and verifying on
        // the real device.
        HardwareAccelerationType::amf
        | HardwareAccelerationType::videotoolbox
        | HardwareAccelerationType::rkmpp
        | HardwareAccelerationType::v4l2m2m
        | HardwareAccelerationType::none
        | HardwareAccelerationType::Unrecognized(_) => return None,
    };
    // The interval, as a frame rate. Upstream computes it into
    // `BaseEncodingJobOptions.MaxFramerate`, which is a `float`, but reads it
    // back out through `GetFramerateParam`, which returns `double?` -- so the
    // number the filter chain prints is the *widened* single-precision value,
    // not the shortest form of either type on its own. A 10 s interval prints
    // `0.10000000149011612`, and Ferrofin's planner already widens the same
    // way (`framerate_param(..).map(f64::from)`).
    #[allow(
        clippy::cast_precision_loss,
        reason = "deliberate: upstream's MaxFramerate is a float, and the \
                  emitted string is that float widened to double"
    )]
    let fps = f64::from(1000.0_f32 / interval_ms as f32);
    let (flag, mut graph) = hw::sw_chain::video_processing_filter_args(
        chain,
        Some(fps),
        hw::sw_chain::StreamPads {
            subtitle_is_external: false,
            subtitle_index: 0,
            video_index: stream.index.max(0),
        },
        false,
        false,
    )?;

    // Upstream inserts `setpts` at the first `fps=` of the filter and throws
    // when there is none. The chain always opens with the `fps=` built from
    // `Some(fps)` above, so a miss cannot happen; were it to, the software
    // path (whose filter always has one) runs instead of failing the job.
    if let Some(rate) = pts_frame_rate {
        let at = graph.find("fps=");
        debug_assert!(at.is_some(), "the trickplay chain opens with fps=: {graph}");
        graph.insert_str(at?, &pts_normalizer(rate));
    }

    let (quality_option, quality) = mjpeg_quality(&video_encoder, qscale);
    let hwaccel_args = hwaccel.args.trim();
    let separator = if hwaccel_args.is_empty() { "" } else { " " };
    // Upstream prepends `-skip_frame nokey ` to the whole input fragment, so it
    // lands ahead of `-init_hw_device`, not between it and `-i`.
    let skip_frame = if keyframe_only {
        " -skip_frame nokey"
    } else {
        ""
    };
    // `GetInputArgument` ends with this whenever a hardware decoder was named.
    // Without it ffmpeg quietly inserts a software scaler behind the hardware
    // decoder as soon as the resolution changes — which is exactly what this
    // path does — handing back a good part of the speedup it just bought.
    let noautoscale = if video_decoder.is_empty() {
        ""
    } else {
        " -noautoscale"
    };
    let sync = crate::encoding_helper::helper::video_sync_option("0", caps.ffmpeg_version());
    // `-noautoscale` belongs to `GetInputArgument`, and upstream appends the
    // map after the whole input argument.
    let map = map_argument(map_index);
    let args = format!(
        "-loglevel error{skip_frame}{separator}{hwaccel_args} -i file:{input}\
         {noautoscale}{map} -an -sn {flag} \"{graph}\" -threads {threads} \
         -c:v {video_encoder} {quality_option} {quality}{sync} -f image2 {output}",
        input = quote(input_path),
        output = quote(output_pattern),
    );
    Some((args, hwaccel.env))
}

/// The MJPEG quality option name and value for `video_encoder`.
///
/// Port of the encoder-quality block of
/// `MediaEncoder.ExtractVideoImagesOnIntervalInternal`. ffmpeg's `-qscale:v`
/// runs 1 (best) to 31 (worst), but the VAAPI and QSV MJPEG encoders take a
/// *JPEG* quality — 0 (worst) to 100 (best) — under a different option name.
/// Passing them `-qscale:v` is not an error, it is worse: they ignore it and
/// encode at the driver default, so the operator's quality setting silently
/// does nothing.
///
/// The `100 / 30` in the C# is **integer** division, so the step is 3 and the
/// top of the range is 100, not 100.0 with a 3.33 step. Reproduce the integer.
///
/// The VideoToolbox and RKMPP arms are not ported; they belong with those
/// vendors' filter chains, which are out of scope (see the `match` above).
fn mjpeg_quality(video_encoder: &str, qscale: i32) -> (&'static str, i32) {
    let quality = qscale.clamp(1, 31);
    let encoder = video_encoder.to_ascii_lowercase();
    if encoder.contains("vaapi") || encoder.contains("qsv") {
        return ("-global_quality:v", 100 - (quality - 1) * (100 / 30));
    }
    ("-qscale:v", quality)
}

/// Whether the configured accelerator can decode keyframes only.
///
/// Port of the `supportsKeyFrameOnly` test in
/// `ExtractVideoImagesOnIntervalAccelerated`. Trickplay wants one frame every
/// few seconds, so decoding only keyframes is most of the speedup — and a
/// decoder that cannot do it would decode *everything*, which is slower than
/// the software path it replaced. Upstream therefore turns hardware off
/// entirely rather than accept that.
#[must_use]
pub fn supports_keyframe_only_decode(caps: &FfmpegCapabilities, options: &EncodingOptions) -> bool {
    match options.hardware_acceleration_type {
        HardwareAccelerationType::nvenc => options.enable_enhanced_nvdec_decoder,
        HardwareAccelerationType::amf => caps.platform().is_windows(),
        HardwareAccelerationType::qsv => options.prefer_system_native_hw_decoder,
        HardwareAccelerationType::vaapi
        | HardwareAccelerationType::videotoolbox
        | HardwareAccelerationType::rkmpp => true,
        _ => false,
    }
}

/// The source height a hardware trickplay scaler should be told about.
///
/// Port of the DAR correction in `ExtractVideoImagesOnIntervalAccelerated`.
/// Hardware scalers take fixed pixel dimensions, so an anamorphic source — one
/// stored stretched, where the stored shape and the display aspect disagree —
/// has to be un-stretched first or every thumbnail comes out the wrong shape.
/// The software scaler avoids this by expressing itself in `dar` terms instead.
#[must_use]
pub fn display_corrected_height(stream: &MediaStream) -> Option<i32> {
    let (width, height) = (stream.width?, stream.height?);
    let dar = stream.aspect_ratio.as_deref().filter(|a| !a.is_empty())?;
    let (wa, ha) = dar.split_once(':')?;
    let (wa, ha) = (
        wa.trim().parse::<f64>().ok()?,
        ha.trim().parse::<f64>().ok()?,
    );
    // Stored square already: nothing to correct.
    if (f64::from(width) * ha - f64::from(height) * wa).abs() <= 0.05 {
        return Some(height);
    }
    // ffprobe reports `0:1` for a stream whose aspect it could not work out,
    // and that clears the guard above (`W * 1 - H * 0` is `W`). Upstream then
    // divides by zero and `Convert.ToInt32(Infinity)` throws; here the cast
    // would saturate to `i32::MAX` and hand a hardware scaler a nonsense
    // height, which is the worse of the two failures. Keep the stored height.
    if wa <= 0.0 {
        return Some(height);
    }
    // SAR = DAR * H / W, so the real height is W / DAR.
    let corrected = f64::from(width) * ha / wa;
    if corrected <= 0.0 {
        return Some(height);
    }
    // `Convert.ToInt32(double)` is banker's rounding, not half-away-from-zero:
    // a 705-wide 2:1 source gives 352.5, which C# turns into 352 and
    // `f64::round` would turn into 353.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "guarded positive and bounded by the frame width above"
    )]
    Some(corrected.round_ties_even() as i32)
}

/// Lists the `.jpg` files directly inside `dir`, sorted by file name.
fn jpg_files_sorted(dir: &Path) -> Result<Vec<String>, ServiceError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        MediaEncodingError::io(format!("cannot read frame directory {}", dir.display()), e)
    })?;
    let mut frames: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
        })
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    frames.sort();
    Ok(frames)
}

#[async_trait]
impl<T: Transcoder> TrickplayFrameExtractor for TrickplayFrameExtractorImpl<T> {
    async fn extract_trickplay_frames(
        &self,
        request: &TrickplayExtraction<'_>,
    ) -> Result<Vec<String>, ServiceError> {
        if request.interval_ms <= 0 {
            return Err(ServiceError::invalid_input(
                "trickplay interval must be positive",
            ));
        }
        if request.max_width <= 0 {
            return Err(ServiceError::invalid_input(
                "trickplay width must be positive",
            ));
        }

        let dir = Path::new(request.output_dir);
        std::fs::create_dir_all(dir).map_err(|e| {
            MediaEncodingError::io(
                format!("cannot create frame directory {}", request.output_dir),
                e,
            )
        })?;
        let output_pattern = dir.join("%08d.jpg");
        let output_pattern = output_pattern.to_string_lossy();

        // `MediaEncoder.cs:937-955`: decided once, from the configured flag, so
        // the standard-way retry below reuses the keyframe-only filter as-is.
        let pts_frame_rate = if request.keyframe_only {
            None
        } else {
            pts_frame_rate(request.video_stream)
        };
        // `MediaEncoder.cs:925-932`: upstream throws when the stream it was
        // asked to thumbnail is not among the source's streams, rather than
        // let ffmpeg choose one.
        let map_index = match request.video_stream {
            Some(stream) => {
                let index =
                    crate::encoding_helper::helper::find_index(request.media_streams, stream);
                if index < 0 {
                    return Err(ServiceError::backend(format!(
                        "unable to locate the requested video stream {} of {}",
                        stream.index, request.input_path
                    )));
                }
                Some(index)
            }
            None => None,
        };
        let plan = self
            .plan(request, &output_pattern, pts_frame_rate, map_index)
            .await;
        let first = self.run(&plan, request.priority).await?;
        if let Some(frames) = Self::frames_of(dir, &first)? {
            return Ok(frames);
        }

        // Upstream retries when the keyframe-only run fails, with the *same*
        // input, filter and encoder arguments minus `-skip_frame nokey` — a
        // hardware job retries on hardware. A file whose keyframe index is
        // broken fails only that way, and decoding every frame still works.
        if request.keyframe_only {
            tracing::warn!(
                path = request.input_path,
                "I-frame trickplay extraction failed, retrying the standard way: {}",
                first.output.trim()
            );
            // Upstream discards the failed run's directory and extracts into a
            // fresh one. The caller owns this directory, so clear it instead:
            // ffmpeg that dies partway leaves frames behind, and mixing them
            // with the retry's would splice two decodes into one tile strip.
            Self::clear_frames(dir)?;
            let retry = self
                .plan(
                    &TrickplayExtraction {
                        keyframe_only: false,
                        ..*request
                    },
                    &output_pattern,
                    pts_frame_rate,
                    map_index,
                )
                .await;
            let second = self.run(&retry, request.priority).await?;
            if let Some(frames) = Self::frames_of(dir, &second)? {
                return Ok(frames);
            }
            // The standard run is the one that explains why the file cannot be
            // read at all; the keyframe-only failure was already logged above.
            return Err(Self::no_frames(request, &second));
        }

        Err(Self::no_frames(request, &first))
    }
}

/// One resolved ffmpeg invocation: the argument line plus the environment it
/// has to run under.
#[derive(Debug)]
struct ExtractionPlan {
    args: String,
    env: Vec<(String, String)>,
    /// Whether this came from the hardware builder. Recorded when the plan is
    /// built rather than sniffed back out of the argument string: an
    /// unknown-vendor VAAPI job is hardware-decoded but carries neither
    /// `-init_hw_device` nor any environment, so the string does not say.
    hardware: bool,
}

impl<T: Transcoder> TrickplayFrameExtractorImpl<T> {
    /// Chooses the hardware plan when everything it needs is present, and the
    /// software one otherwise.
    async fn plan(
        &self,
        request: &TrickplayExtraction<'_>,
        output_pattern: &str,
        pts_frame_rate: Option<f32>,
        map_index: Option<i32>,
    ) -> ExtractionPlan {
        if request.allow_hw_accel
            && let Some(stream) = request.video_stream
            && let Some(options) = self.encoding_options().await
            && let Some((args, env)) = build_accelerated_trickplay_args(
                &self.caps,
                &options,
                &TrickplayJob {
                    input_path: request.input_path,
                    stream,
                    interval_ms: request.interval_ms,
                    max_width: request.max_width,
                    qscale: request.qscale,
                    threads: request.threads,
                    output_pattern,
                    allow_hw_accel: request.allow_hw_accel,
                    enable_hw_encoding: request.enable_hw_encoding,
                    keyframe_only: request.keyframe_only,
                    pts_frame_rate,
                    map_index,
                },
            )
        {
            return ExtractionPlan {
                args,
                env,
                hardware: true,
            };
        }
        self.software_plan(request, output_pattern, pts_frame_rate, map_index)
    }

    /// The software plan, which needs nothing probed.
    fn software_plan(
        &self,
        request: &TrickplayExtraction<'_>,
        output_pattern: &str,
        pts_frame_rate: Option<f32>,
        map_index: Option<i32>,
    ) -> ExtractionPlan {
        ExtractionPlan {
            args: build_trickplay_args(
                request.input_path,
                request.interval_ms,
                request.max_width,
                request.qscale,
                request.threads,
                output_pattern,
                self.ffmpeg_version,
                request.keyframe_only,
                pts_frame_rate,
                map_index,
            ),
            env: Vec::new(),
            hardware: false,
        }
    }

    /// The frames a finished run produced, or `None` if it did not succeed.
    ///
    /// Both halves matter. Upstream keys on `!ranToCompletion || ExitCode != 0`
    /// and notes in its own comment that a failed ffmpeg is *not* guaranteed to
    /// leave the directory empty — so a non-zero exit with frames on disk is a
    /// truncated extraction, not a success, and treating it as one would splice
    /// a half-decoded strip into the scrub bar with nothing logged.
    fn frames_of(
        dir: &Path,
        run: &crate::encoder::ProcessOutput,
    ) -> Result<Option<Vec<String>>, ServiceError> {
        if !run.success {
            return Ok(None);
        }
        let frames = jpg_files_sorted(dir)?;
        Ok((!frames.is_empty()).then_some(frames))
    }

    /// Removes the JPEGs a failed run left behind, before a retry reuses the
    /// directory.
    fn clear_frames(dir: &Path) -> Result<(), ServiceError> {
        for frame in jpg_files_sorted(dir)? {
            if let Err(e) = std::fs::remove_file(&frame) {
                return Err(MediaEncodingError::io(
                    format!("cannot clear partial trickplay frame {frame}"),
                    e,
                )
                .into());
            }
        }
        Ok(())
    }

    /// The error for a run that produced nothing usable.
    fn no_frames(
        request: &TrickplayExtraction<'_>,
        run: &crate::encoder::ProcessOutput,
    ) -> ServiceError {
        ServiceError::backend(format!(
            "ffmpeg produced no trickplay frames for {}: {}",
            request.input_path,
            run.output.trim()
        ))
    }

    /// Runs one plan, returning its captured stderr and exit status.
    async fn run(
        &self,
        plan: &ExtractionPlan,
        priority: ferrofin_model::configuration::ProcessPriorityClass,
    ) -> Result<crate::encoder::ProcessOutput, ServiceError> {
        // The same convention the transcode manager follows: at debug, the
        // exact argument line. Whether a job took the hardware path is
        // otherwise invisible from the outside -- a software fallback and a
        // GPU run produce the same JPEGs, just at different speeds.
        tracing::debug!(
            ffmpeg_args = %plan.args,
            hardware = plan.hardware,
            "trickplay ffmpeg arguments"
        );
        let _permit = self
            .image_encoding_pool
            .acquire()
            .await
            .map_err(ServiceError::backend_source)?;
        self.transcoder
            .get_process_output_at_priority(
                &self.ffmpeg_path,
                &plan.args,
                true,
                &plan.env,
                priority,
            )
            .await
            .map_err(|e| MediaEncodingError::process(e).into())
    }
}

#[cfg(test)]
mod tests {
    use super::jpg_files_sorted;
    use super::{
        EncodingOptions, HardwareAccelerationType, MediaStream, ServerConfigurationManager,
        ServiceError, TrickplayExtraction,
    };
    use crate::encoder::FfmpegVersion;
    use crate::encoder::ProcessOutput;
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ferrofin_traits::media_encoding::TrickplayFrameExtractor as _;

    use super::{TrickplayFrameExtractorImpl, build_trickplay_args};
    use crate::encoder::Transcoder;

    /// A [`Transcoder`] fake that records the argument line and "produces" the
    /// given frame files by touching them in the parsed output directory.
    struct RecordingTranscoder {
        args: Mutex<Vec<String>>,
        env: Mutex<Vec<Vec<(String, String)>>>,
        /// The priority each run was asked for.
        priorities: Mutex<Vec<ferrofin_model::configuration::ProcessPriorityClass>>,
        frames_to_write: usize,
        stderr: String,
        /// Whether the fake ffmpeg exits `0`. Independent of how many frames
        /// it wrote, because that is the case that matters: ffmpeg dying
        /// partway exits non-zero **with** frames already on disk.
        exit_ok: bool,
    }

    impl RecordingTranscoder {
        fn new(frames_to_write: usize, stderr: &str) -> Self {
            Self {
                args: Mutex::new(Vec::new()),
                env: Mutex::new(Vec::new()),
                priorities: Mutex::new(Vec::new()),
                frames_to_write,
                stderr: stderr.to_owned(),
                exit_ok: true,
            }
        }

        /// The same fake, but the process exits non-zero.
        fn failing(mut self) -> Self {
            self.exit_ok = false;
            self
        }

        fn recorded(&self) -> Vec<String> {
            self.args.lock().expect("args lock").clone()
        }

        /// The environment each recorded run was given.
        fn recorded_env(&self) -> Vec<Vec<(String, String)>> {
            self.env.lock().expect("env lock").clone()
        }
    }

    #[async_trait]
    impl Transcoder for RecordingTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
            env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            self.args
                .lock()
                .expect("args lock")
                .push(arguments.to_owned());
            self.env.lock().expect("env lock").push(env.to_vec());
            // Recover the output dir from the trailing `"{dir}/%08d.jpg"`.
            let argv = crate::encoder::split_args(arguments)?;
            let pattern = argv.last().expect("output pattern present");
            let dir = std::path::Path::new(pattern)
                .parent()
                .expect("pattern has a parent dir");
            // A keyframe-only run that writes nothing is how upstream's retry
            // is triggered; `frames_to_write` of 0 models exactly that.
            for i in 1..=self.frames_to_write {
                std::fs::write(dir.join(format!("{i:08}.jpg")), b"jpg").expect("write frame");
            }
            Ok(ProcessOutput {
                output: self.stderr.clone(),
                success: self.exit_ok,
            })
        }

        async fn get_process_output_at_priority(
            &self,
            path: &str,
            arguments: &str,
            read_stderr: bool,
            env: &[(String, String)],
            priority: ferrofin_model::configuration::ProcessPriorityClass,
        ) -> Result<ProcessOutput, String> {
            self.priorities
                .lock()
                .expect("priorities lock")
                .push(priority);
            self.get_process_output(path, arguments, read_stderr, None, env)
                .await
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    #[test]
    fn args_mirror_the_upstream_format_string() {
        let args = build_trickplay_args(
            "/m/v.mkv",
            10_000,
            320,
            4,
            1,
            "/tmp/out/%08d.jpg",
            Some(FfmpegVersion::with_build(7, 0, 1)),
            false,
            None,
            None,
        );
        assert_eq!(
            args,
            "-loglevel error -threads 1 -i file:\"/m/v.mkv\" -an -sn \
             -vf \"fps=1000/10000,scale=trunc(min(max(iw\\,ih*dar)\\,320)/2)*2:trunc(ow/dar/2)*2\" \
             -threads 1 -c:v mjpeg -qscale:v 4 -fps_mode passthrough -f image2 \"/tmp/out/%08d.jpg\""
        );
    }

    /// A file name built to close the quote must stay one `-i` value: before
    /// the fix this path ended the `file:"…"` span and `-y -i /etc/passwd`
    /// reached ffmpeg as options.
    #[rstest::rstest]
    #[case::quote(r#"/m/a" -y -i /etc/passwd ".mkv"#)]
    #[case::backslash_quote(r#"/m/a\" -y -i /etc/passwd \".mkv"#)]
    fn a_hostile_path_stays_one_input(#[case] path: &str) {
        let args = build_trickplay_args(
            path,
            10_000,
            320,
            4,
            1,
            "/tmp/o/%08d.jpg",
            None,
            false,
            None,
            None,
        );
        let tokens = crate::encoder::split_args(&args).unwrap();
        let i = tokens.iter().position(|a| a == "-i").unwrap();
        assert_eq!(tokens[i + 1], format!("file:{path}"));
        assert_eq!(tokens.iter().filter(|a| *a == "-i").count(), 1);
        assert_eq!(tokens.last().unwrap(), "/tmp/o/%08d.jpg");
    }

    #[test]
    fn software_extraction_can_skip_to_keyframes_too() {
        // Upstream prepends `-skip_frame nokey` to the input fragment whether
        // or not hardware is in play, so a job that dropped to software
        // because its accelerator cannot skip keyframes still skips them here.
        let args = build_trickplay_args("/m/v.mkv", 10_000, 320, 4, 1, "o", None, true, None, None);
        assert!(
            args.starts_with("-loglevel error -skip_frame nokey -threads 1 -i "),
            "{args}"
        );
        let args =
            build_trickplay_args("/m/v.mkv", 10_000, 320, 4, 1, "o", None, false, None, None);
        assert!(!args.contains("-skip_frame"), "{args}");
    }

    #[test]
    fn qscale_is_clamped_to_ffmpeg_range() {
        let args =
            build_trickplay_args("/m/v.mkv", 10_000, 320, 0, 0, "o", None, false, None, None);
        assert!(args.contains("-qscale:v 1 "), "low clamp in {args}");
        let args =
            build_trickplay_args("/m/v.mkv", 10_000, 320, 99, 0, "o", None, false, None, None);
        assert!(args.contains("-qscale:v 31 "), "high clamp in {args}");
    }

    /// A software-path request: no video stream, hardware switched off.
    fn request<'a>(input_path: &'a str, output_dir: &'a str) -> TrickplayExtraction<'a> {
        TrickplayExtraction {
            input_path,
            video_stream: None,
            media_streams: &[],
            interval_ms: 10_000,
            max_width: 320,
            qscale: 4,
            threads: 1,
            priority: ferrofin_model::configuration::ProcessPriorityClass::BelowNormal,
            output_dir,
            allow_hw_accel: false,
            enable_hw_encoding: false,
            keyframe_only: false,
        }
    }

    /// `TrickplayOptions.ProcessPriority` reaches the ffmpeg run
    /// (`ExtractVideoImagesOnIntervalInternal`'s `priority`).
    #[tokio::test]
    async fn the_configured_priority_reaches_the_ffmpeg_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(1, ""));
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None);
        let out_dir = out.to_string_lossy();
        let req = ferrofin_traits::media_encoding::TrickplayExtraction {
            priority: ferrofin_model::configuration::ProcessPriorityClass::Idle,
            ..request("/m/v.mkv", &out_dir)
        };
        extractor
            .extract_trickplay_frames(&req)
            .await
            .expect("frames");
        assert_eq!(
            *transcoder.priorities.lock().expect("priorities lock"),
            [ferrofin_model::configuration::ProcessPriorityClass::Idle]
        );
    }

    #[tokio::test]
    async fn extraction_returns_sorted_frames() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(3, ""));
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None);

        let frames = extractor
            .extract_trickplay_frames(&request("/m/v.mkv", &out.to_string_lossy()))
            .await
            .expect("frames");

        assert_eq!(frames.len(), 3);
        assert!(frames[0].ends_with("00000001.jpg"));
        assert!(frames[2].ends_with("00000003.jpg"));
        assert!(frames.windows(2).all(|w| w[0] < w[1]), "sorted order");

        let recorded = transcoder.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains("fps=1000/10000"));
        assert!(recorded[0].contains("-i file:\"/m/v.mkv\""));
    }

    #[tokio::test]
    async fn no_frames_is_an_error_carrying_stderr() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let extractor = TrickplayFrameExtractorImpl::new(
            Arc::new(RecordingTranscoder::new(0, "boom: no such codec")),
            "ffmpeg",
            None,
        );

        let err = extractor
            .extract_trickplay_frames(&request("/m/v.mkv", &out.to_string_lossy()))
            .await
            .expect_err("no frames should error");
        assert!(err.to_string().contains("boom: no such codec"), "{err}");
    }

    /// A config handle returning fixed encoding options.
    #[derive(Debug)]
    struct FixedEncoding(EncodingOptions);

    #[async_trait]
    impl ServerConfigurationManager for FixedEncoding {
        async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
            Ok(self.0.clone())
        }

        // The extractor reads exactly one thing off this trait. The rest are
        // unreachable here, and saying so is better than inventing plausible
        // values a future test might accidentally rely on.
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unimplemented!("trickplay reads only the encoding options")
        }
        async fn configuration(
            &self,
        ) -> Result<Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError> {
            unimplemented!("trickplay reads only the encoding options")
        }
        async fn update_configuration(
            &self,
            _configuration: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            unimplemented!("trickplay reads only the encoding options")
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            unimplemented!("trickplay reads only the encoding options")
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("trickplay reads only the encoding options")
        }
    }

    /// The VAAPI-on-Linux extractor the wiring tests run against.
    fn hardware_extractor(
        transcoder: Arc<RecordingTranscoder>,
    ) -> TrickplayFrameExtractorImpl<RecordingTranscoder> {
        let caps = crate::encoding_helper::hw::FfmpegCapabilities::builder()
            .platform(crate::encoding_helper::hw::Platform::Linux)
            .encoders(["mjpeg_vaapi", "mjpeg"])
            .hwaccels(["vaapi"])
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .vaapi_driver(false, true, false)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            enable_hardware_encoding: true,
            // See the fixture note in `accelerated_tests`: a real render node
            // would make this depend on the test machine having a GPU.
            vaapi_device: Some("/dev/null".to_owned()),
            hardware_decoding_codecs: vec!["h264".to_owned()],
            ..EncodingOptions::default()
        };
        TrickplayFrameExtractorImpl::new(transcoder, "ffmpeg", None)
            .with_hardware(Arc::new(caps), Arc::new(FixedEncoding(options)))
    }

    /// A request that asks for everything the hardware path offers.
    fn hardware_request<'a>(
        stream: &'a MediaStream,
        output_dir: &'a str,
    ) -> TrickplayExtraction<'a> {
        TrickplayExtraction {
            video_stream: Some(stream),
            media_streams: std::slice::from_ref(stream),
            allow_hw_accel: true,
            enable_hw_encoding: true,
            keyframe_only: true,
            ..request("/m/v.mkv", output_dir)
        }
    }

    fn h264_stream() -> MediaStream {
        MediaStream {
            codec: Some("h264".to_owned()),
            index: 0,
            stream_type: ferrofin_model::entities::MediaStreamType::Video,
            pixel_format: Some("yuv420p".to_owned()),
            width: Some(1920),
            height: Some(1080),
            ..MediaStream::default()
        }
    }

    /// A video stream whose probed frame rate is `fps` (`None`: ffprobe read
    /// none).
    fn stream_at(fps: Option<f32>) -> MediaStream {
        MediaStream {
            average_frame_rate: fps,
            real_frame_rate: fps,
            ..h264_stream()
        }
    }

    /// v12.2 `ExtractVideoImagesOnIntervalAccelerated` (:937-955, PR 17304):
    /// with keyframe-only extraction off, `setpts=N/{rate:F3}/TB,` goes in
    /// front of the `fps=` sampler, so broken container timestamps cannot make
    /// it emit frames without bound. The rate is the stream's
    /// `ReferenceFrameRate`. A stream without one has no `fps=` upstream (and
    /// fails there); Ferrofin samples it on its container timestamps.
    #[rstest::rstest]
    #[case::film(Some(23.976), "setpts=N/23.976/TB,fps=")]
    #[case::ntsc(Some(29.97), "setpts=N/29.970/TB,fps=")]
    #[case::no_frame_rate(None, "fps=")]
    #[tokio::test]
    async fn a_full_decode_rebuilds_timestamps_before_sampling(
        #[case] fps: Option<f32>,
        #[case] expected: &str,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None);
        let stream = stream_at(fps);

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                video_stream: Some(&stream),
                media_streams: std::slice::from_ref(&stream),
                ..request("/m/v.mkv", &out.to_string_lossy())
            })
            .await
            .expect("frames");

        let recorded = transcoder.recorded();
        assert!(
            recorded[0].contains(&format!("-vf \"{expected}")),
            "{}",
            recorded[0]
        );
    }

    /// Upstream builds the filter once from the CONFIGURED keyframe-only flag
    /// and reuses it for the standard-way retry (`MediaEncoder.cs:980`), so
    /// with keyframe-only on neither the first run nor its retry gets setpts.
    #[tokio::test]
    async fn keyframe_only_runs_and_their_retry_keep_the_plain_sampler() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(0, "no keyframes").failing());
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None);
        let stream = stream_at(Some(25.0));

        let _ = extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                video_stream: Some(&stream),
                media_streams: std::slice::from_ref(&stream),
                keyframe_only: true,
                ..request("/m/v.mkv", &out.to_string_lossy())
            })
            .await;

        let recorded = transcoder.recorded();
        assert_eq!(recorded.len(), 2, "the keyframe-only run and its retry");
        for args in &recorded {
            assert!(!args.contains("setpts=N/"), "{args}");
            assert!(args.contains("-vf \"fps="), "{args}");
        }
        assert!(!recorded[1].contains("-skip_frame"), "{}", recorded[1]);
    }

    /// A cover image stored ahead of the main video, then the audio: the
    /// layout that made ffmpeg pick the wrong stream upstream.
    fn covered_streams() -> Vec<MediaStream> {
        vec![
            MediaStream {
                codec: Some("mjpeg".to_owned()),
                index: 0,
                stream_type: ferrofin_model::entities::MediaStreamType::EmbeddedImage,
                width: Some(3840),
                height: Some(2160),
                ..MediaStream::default()
            },
            MediaStream {
                index: 1,
                ..stream_at(Some(25.0))
            },
            MediaStream {
                codec: Some("aac".to_owned()),
                index: 2,
                stream_type: ferrofin_model::entities::MediaStreamType::Audio,
                ..MediaStream::default()
            },
        ]
    }

    /// v12.2 `ExtractVideoImagesOnIntervalAccelerated` (:925-932, PR 17883):
    /// "Force the video stream, otherwise ffmpeg may pick a cover image." The
    /// input argument ends with ` -map 0:{FindIndex}`, so ffmpeg decodes the
    /// stream the job was sized for, not the higher-resolution cover.
    #[tokio::test]
    async fn a_full_decode_maps_the_video_stream_it_was_sized_for() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None);
        let streams = covered_streams();

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                video_stream: Some(&streams[1]),
                media_streams: &streams,
                ..request("/m/v.mkv", &out.to_string_lossy())
            })
            .await
            .expect("frames");

        let recorded = transcoder.recorded();
        assert!(
            recorded[0].contains("-i file:\"/m/v.mkv\" -map 0:1 -an -sn"),
            "{}",
            recorded[0]
        );
    }

    /// Upstream throws "Unable to locate requested stream" rather than let
    /// ffmpeg pick a stream when the one it was asked to thumbnail is not
    /// among the source's streams; nothing is spawned.
    #[tokio::test]
    async fn a_video_stream_missing_from_the_source_fails_the_job() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None);
        let streams = covered_streams();
        let elsewhere = MediaStream {
            index: 7,
            ..stream_at(Some(25.0))
        };

        let err = extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                video_stream: Some(&elsewhere),
                media_streams: &streams,
                ..request("/m/v.mkv", &out.to_string_lossy())
            })
            .await
            .expect_err("an unknown stream is not guessed at");

        assert!(err.to_string().contains("unable to locate"), "{err}");
        assert!(transcoder.recorded().is_empty(), "ffmpeg never ran");
    }

    /// On the GPU the map follows the whole input argument, `-noautoscale`
    /// included, as upstream appends it to `GetInputArgument`'s result.
    #[tokio::test]
    async fn a_gpu_decode_maps_the_video_stream_after_the_input() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect("frames");

        let recorded = transcoder.recorded();
        assert!(
            recorded[0].contains(" -noautoscale -map 0:0 -an -sn"),
            "{}",
            recorded[0]
        );
    }

    /// The hardware chain gets the same splice: `setpts` lands in front of the
    /// `fps=` at the head of the chain.
    #[tokio::test]
    async fn a_full_decode_on_the_gpu_rebuilds_timestamps_too() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = stream_at(Some(23.976));

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                keyframe_only: false,
                ..hardware_request(&stream, &out.to_string_lossy())
            })
            .await
            .expect("frames");

        let recorded = transcoder.recorded();
        assert!(
            recorded[0].contains("-init_hw_device vaapi=va:"),
            "{}",
            recorded[0]
        );
        assert!(
            recorded[0].contains("\"setpts=N/23.976/TB,fps="),
            "{}",
            recorded[0]
        );
    }

    #[tokio::test]
    async fn a_wired_hardware_job_runs_on_the_gpu() {
        // The whole point of the wiring: with the trickplay hardware switch on
        // and a video stream to size from, the extractor must actually spawn
        // the accelerated command -- not merely be capable of building it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect("frames");

        let recorded = transcoder.recorded();
        assert_eq!(recorded.len(), 1, "one run, no retry");
        assert!(
            recorded[0].contains("-init_hw_device vaapi=va:"),
            "{}",
            recorded[0]
        );
        assert!(recorded[0].contains("-c:v mjpeg_vaapi"), "{}", recorded[0]);
        assert!(recorded[0].contains("-skip_frame nokey"), "{}", recorded[0]);
    }

    #[tokio::test]
    async fn the_libva_driver_reaches_the_spawned_child() {
        // The end-to-end half of the unit golden: building the environment is
        // useless if the extractor drops it on the way to the process. An
        // i965 host that loses `LIBVA_DRIVER_NAME` silently runs on iHD.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let caps = crate::encoding_helper::hw::FfmpegCapabilities::builder()
            .platform(crate::encoding_helper::hw::Platform::Linux)
            .encoders(["mjpeg_vaapi", "mjpeg"])
            .hwaccels(["vaapi"])
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .vaapi_driver(false, false, true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            enable_hardware_encoding: true,
            vaapi_device: Some("/dev/null".to_owned()),
            hardware_decoding_codecs: vec!["h264".to_owned()],
            ..EncodingOptions::default()
        };
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None)
            .with_hardware(Arc::new(caps), Arc::new(FixedEncoding(options)));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect("frames");

        let env = transcoder.recorded_env();
        assert_eq!(env.len(), 1);
        assert!(
            env[0].contains(&("LIBVA_DRIVER_NAME".to_owned(), "i965".to_owned())),
            "{:?}",
            env[0]
        );
    }

    #[tokio::test]
    async fn a_job_with_no_video_stream_falls_back_to_software() {
        // Hardware is allowed, but nothing describes the source, so a decoder
        // cannot be chosen. Degrading is correct; refusing the job is not.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                allow_hw_accel: true,
                ..request("/m/v.mkv", &out.to_string_lossy())
            })
            .await
            .expect("frames");

        let recorded = transcoder.recorded();
        assert!(!recorded[0].contains("-init_hw_device"), "{}", recorded[0]);
        assert!(recorded[0].contains("-c:v mjpeg "), "{}", recorded[0]);
    }

    #[tokio::test]
    async fn the_trickplay_switch_overrides_the_playback_one() {
        // Playback runs on VAAPI, but trickplay hardware is switched off. The
        // job must stay on the CPU: these are separate settings and reading
        // the wrong one is how an operator's choice gets ignored.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                allow_hw_accel: false,
                ..hardware_request(&stream, &out.to_string_lossy())
            })
            .await
            .expect("frames");

        assert!(!transcoder.recorded()[0].contains("-init_hw_device"));
    }

    #[tokio::test]
    async fn a_broken_keyframe_index_retries_without_skip_frame() {
        // Upstream's fallback: a file whose keyframe index is broken produces
        // no frames under `-skip_frame nokey` and only that way, so the same
        // job is retried once without it before the run is called a failure.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(0, "no frames decoded").failing());
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        let out_dir = out.to_string_lossy();
        let req = ferrofin_traits::media_encoding::TrickplayExtraction {
            priority: ferrofin_model::configuration::ProcessPriorityClass::Idle,
            ..hardware_request(&stream, &out_dir)
        };
        let err = extractor
            .extract_trickplay_frames(&req)
            .await
            .expect_err("both runs produce nothing");

        let recorded = transcoder.recorded();
        assert_eq!(recorded.len(), 2, "the retry ran: {recorded:?}");
        // The retry runs at the configured priority too.
        assert_eq!(
            *transcoder.priorities.lock().expect("priorities lock"),
            [ferrofin_model::configuration::ProcessPriorityClass::Idle; 2]
        );
        assert!(recorded[0].contains("-skip_frame nokey"), "{}", recorded[0]);
        assert!(!recorded[1].contains("-skip_frame"), "{}", recorded[1]);
        // The retry stays on hardware. Upstream passes the same input, filter
        // and encoder arguments to its second run -- dropping to software here
        // would quietly give up the GPU on every file with a bad index.
        assert!(recorded[1].contains("-hwaccel vaapi"), "{}", recorded[1]);
        assert!(err.to_string().contains("no frames decoded"), "{err}");
    }

    #[tokio::test]
    async fn a_truncated_run_is_a_failure_even_though_it_wrote_frames() {
        // The case an empty-directory check cannot see: ffmpeg decodes a few
        // keyframes, dies, and exits non-zero with frames already on disk.
        // Upstream keys its retry on the exit code and deletes the partial
        // output; treating "some JPEGs exist" as success would splice a
        // half-decoded strip into the scrub bar with nothing logged.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, "decoder gave up").failing());
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect_err("a non-zero exit is a failure whatever it wrote");

        assert_eq!(transcoder.recorded().len(), 2, "the retry still ran");
        // Both runs wrote `00000001.jpg`/`00000002.jpg`; only the retry's
        // survive, because the first run's were cleared before it started.
        let frames = jpg_files_sorted(&out).expect("list frames");
        assert_eq!(frames.len(), 2, "no frames from the failed run remain");
    }

    #[tokio::test]
    async fn a_failed_run_without_keyframe_only_does_not_retry() {
        // Upstream rethrows immediately when keyframe-only was not in play:
        // there is no second thing to try.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(0, "bad input").failing());
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                keyframe_only: false,
                ..hardware_request(&stream, &out.to_string_lossy())
            })
            .await
            .expect_err("the run failed");

        assert_eq!(transcoder.recorded().len(), 1, "no retry");
    }

    #[tokio::test]
    async fn the_reported_error_is_the_standard_runs_not_the_keyframe_one() {
        // The standard run explains why the file cannot be read at all; the
        // keyframe-only failure is already known and logged as a warning.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(0, "second run says why").failing());
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        let err = extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect_err("both runs fail");
        assert!(err.to_string().contains("second run says why"), "{err}");
    }

    #[tokio::test]
    async fn an_accelerator_that_cannot_skip_keyframes_keeps_the_flag_in_software() {
        // Upstream turns *hardware* off when the decoder cannot skip to
        // keyframes -- it does not turn keyframe-only off. The software run
        // must still carry `-skip_frame nokey`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let caps = crate::encoding_helper::hw::FfmpegCapabilities::builder()
            .platform(crate::encoding_helper::hw::Platform::Linux)
            .encoders(["h264_nvenc", "mjpeg"])
            .hwaccels(["cuda"])
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
            // The gate: nvenc needs the enhanced decoder to skip to keyframes.
            enable_enhanced_nvdec_decoder: false,
            enable_hardware_encoding: true,
            hardware_decoding_codecs: vec!["h264".to_owned()],
            ..EncodingOptions::default()
        };
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None)
            .with_hardware(Arc::new(caps), Arc::new(FixedEncoding(options)));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect("frames");

        let args = &transcoder.recorded()[0];
        assert!(!args.contains("-init_hw_device"), "software: {args}");
        assert!(
            args.contains("-skip_frame nokey"),
            "keyframe-only kept: {args}"
        );
    }

    #[tokio::test]
    async fn hardware_decode_with_a_software_encoder_is_expressible() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                enable_hw_encoding: false,
                ..hardware_request(&stream, &out.to_string_lossy())
            })
            .await
            .expect("frames");

        let args = &transcoder.recorded()[0];
        assert!(args.contains("-hwaccel vaapi"), "decode on the GPU: {args}");
        assert!(args.contains("-c:v mjpeg "), "encode on the CPU: {args}");
    }

    #[tokio::test]
    async fn an_unreadable_encoding_config_degrades_to_software() {
        /// A config handle whose encoding options cannot be read.
        #[derive(Debug)]
        struct BrokenEncoding;

        #[async_trait]
        impl ServerConfigurationManager for BrokenEncoding {
            async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
                Err(ServiceError::backend("encoding.json is not readable"))
            }
            fn application_paths(
                &self,
            ) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
                unimplemented!("trickplay reads only the encoding options")
            }
            async fn configuration(
                &self,
            ) -> Result<Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError>
            {
                unimplemented!("trickplay reads only the encoding options")
            }
            async fn update_configuration(
                &self,
                _configuration: &ferrofin_model::configuration::ServerConfiguration,
            ) -> Result<(), ServiceError> {
                unimplemented!("trickplay reads only the encoding options")
            }
            async fn get_branding(
                &self,
            ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
                unimplemented!("trickplay reads only the encoding options")
            }
            async fn update_branding(
                &self,
                _branding: &ferrofin_model::branding::BrandingOptions,
            ) -> Result<(), ServiceError> {
                unimplemented!("trickplay reads only the encoding options")
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(2, ""));
        let caps = crate::encoding_helper::hw::FfmpegCapabilities::default();
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", None)
            .with_hardware(Arc::new(caps), Arc::new(BrokenEncoding));
        let stream = h264_stream();

        // Trickplay must not die because a config file is unreadable.
        extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect("frames");
        assert!(!transcoder.recorded()[0].contains("-init_hw_device"));
    }

    #[tokio::test]
    async fn a_run_that_succeeds_first_time_does_not_retry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(3, ""));
        let extractor = hardware_extractor(Arc::clone(&transcoder));
        let stream = h264_stream();

        let frames = extractor
            .extract_trickplay_frames(&hardware_request(&stream, &out.to_string_lossy()))
            .await
            .expect("frames");

        assert_eq!(frames.len(), 3);
        assert_eq!(transcoder.recorded().len(), 1);
    }

    #[tokio::test]
    async fn non_positive_inputs_are_invalid() {
        let extractor = TrickplayFrameExtractorImpl::new(
            Arc::new(RecordingTranscoder::new(0, "")),
            "ffmpeg",
            None,
        );
        assert!(
            extractor
                .extract_trickplay_frames(&TrickplayExtraction {
                    interval_ms: 0,
                    ..request("/m/v.mkv", "/tmp/x")
                })
                .await
                .is_err()
        );
        assert!(
            extractor
                .extract_trickplay_frames(&TrickplayExtraction {
                    max_width: 0,
                    ..request("/m/v.mkv", "/tmp/x")
                })
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod accelerated_tests {
    use super::*;
    use crate::encoder::{FfmpegVersion, REQUIRED_FILTERS};
    use crate::encoding_helper::hw::Platform;
    use ferrofin_model::entities::MediaStreamType;

    fn caps(platform: Platform, driver_ihd: bool) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(platform)
            .encoders(["mjpeg_vaapi", "mjpeg_qsv", "mjpeg"])
            .hwaccels(["vaapi", "qsv", "cuda", "opencl", "drm", "d3d11va"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .vaapi_driver(false, driver_ihd, false)
            .os_version(FfmpegVersion::new(6, 1))
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build()
    }

    fn stream(width: i32, height: i32, dar: Option<&str>) -> MediaStream {
        MediaStream {
            codec: Some("h264".to_owned()),
            index: 0,
            stream_type: MediaStreamType::Video,
            pixel_format: Some("yuv420p".to_owned()),
            width: Some(width),
            height: Some(height),
            aspect_ratio: dar.map(str::to_owned),
            ..MediaStream::default()
        }
    }

    /// A job with every trickplay switch on, which is what an operator who
    /// ticked "hardware acceleration" for trickplay gets.
    fn job<'a>(stream: &'a MediaStream, output_pattern: &'a str) -> TrickplayJob<'a> {
        TrickplayJob {
            input_path: "/m/v.mkv",
            stream,
            interval_ms: 10_000,
            max_width: 320,
            qscale: 4,
            threads: 1,
            output_pattern,
            allow_hw_accel: true,
            enable_hw_encoding: true,
            keyframe_only: true,
            pts_frame_rate: None,
            map_index: None,
        }
    }

    fn vaapi_options() -> EncodingOptions {
        EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            enable_hardware_encoding: true,
            // `/dev/null` rather than a real `/dev/dri/renderD*`: the node is
            // resolved with `fs::metadata`, so naming a real render node makes
            // the expected argument depend on whether the machine running the
            // tests has a GPU. It does not on CI. `/dev/null` is a character
            // device, the same class of node, and exists everywhere.
            vaapi_device: Some("/dev/null".to_owned()),
            hardware_decoding_codecs: vec!["h264".to_owned()],
            ..EncodingOptions::default()
        }
    }

    #[test]
    fn a_vaapi_trickplay_run_decodes_keyframes_on_the_gpu() {
        let caps = caps(Platform::Linux, true);
        let options = vaapi_options();
        let (args, env) = build_accelerated_trickplay_args(
            &caps,
            &options,
            &job(&stream(1920, 1080, None), "/tmp/out/%08d.jpg"),
        )
        .expect("vaapi supports keyframe-only decoding");
        // iHD is libva's own first choice, so nothing has to be forced.
        assert!(env.is_empty(), "{env:?}");
        // The speedup: only keyframes are decoded at all.
        assert!(args.contains("-skip_frame nokey"), "{args}");
        assert!(
            args.contains("-init_hw_device vaapi=va:/dev/null,driver=iHD"),
            "{args}"
        );
        // The interval, expressed as the frame rate at the head of the chain.
        // The float-widened-to-double MaxFramerate, exactly as upstream
        // prints it -- not the shortest form of the f32 (`0.1`).
        assert!(args.contains("fps=0.10000000149011612,"), "{args}");
        assert!(args.contains("scale_vaapi="), "{args}");
        assert!(args.contains("-c:v mjpeg_vaapi"), "{args}");
        // `mjpeg_vaapi` takes a JPEG quality (0 worst, 100 best) under its own
        // option name; handing it `-qscale:v` is not an error, it is silently
        // ignored, so the operator's setting would do nothing. 100 - (4-1)*3.
        assert!(
            args.contains("-global_quality:v 91 -fps_mode passthrough -f image2"),
            "{args}"
        );
        // ffmpeg would otherwise splice a software scaler in behind the
        // hardware decoder the moment the resolution changes, which is the one
        // thing this whole path exists to do.
        assert!(args.contains(" -noautoscale "), "{args}");
        // Upstream prepends the keyframe flag to the whole input fragment, so
        // it sits ahead of `-init_hw_device`, not between it and `-i`.
        assert!(
            args.starts_with("-loglevel error -skip_frame nokey -init_hw_device"),
            "{args}"
        );
    }

    #[test]
    fn the_trickplay_switches_are_read_instead_of_the_playback_ones() {
        let caps = caps(Platform::Linux, true);
        let options = vaapi_options();
        let stream = stream(1920, 1080, None);

        // Hardware off *for trickplay* stays off even though playback uses it.
        let mut off = job(&stream, "/tmp/o/%08d.jpg");
        off.allow_hw_accel = false;
        assert!(build_accelerated_trickplay_args(&caps, &options, &off).is_none());

        // Hardware decode, software MJPEG encoder: a real configuration, and
        // the one that regresses if this reads the global encoding option.
        let mut sw_encode = job(&stream, "/tmp/o/%08d.jpg");
        sw_encode.enable_hw_encoding = false;
        let (args, _) = build_accelerated_trickplay_args(&caps, &options, &sw_encode)
            .expect("hardware decode is still on");
        assert!(args.contains("-c:v mjpeg "), "{args}");
        assert!(!args.contains("mjpeg_vaapi"), "{args}");
        // A software encoder wants ffmpeg's own qscale back.
        assert!(args.contains("-qscale:v 4 "), "{args}");

        // Keyframe-only off: hardware is still worth having, it just decodes
        // every frame. Gating unconditionally here would lose the GPU.
        let mut every_frame = job(&stream, "/tmp/o/%08d.jpg");
        every_frame.keyframe_only = false;
        let (args, _) = build_accelerated_trickplay_args(&caps, &options, &every_frame)
            .expect("hardware does not depend on keyframe-only mode");
        assert!(!args.contains("-skip_frame"), "{args}");
        assert!(args.contains("-init_hw_device"), "{args}");
    }

    #[test]
    fn every_supported_accelerator_builds_a_chain() {
        let caps = caps(Platform::Linux, true);
        let stream = stream(1920, 1080, None);
        for (accel, marker) in [
            (HardwareAccelerationType::vaapi, "-init_hw_device vaapi=va:"),
            // QSV on Linux with the native decoder preferred runs the VAAPI
            // chain and hands the frames over, so `scale_vaapi` is NOT the
            // marker that tells the two apart -- the device handover is.
            (HardwareAccelerationType::qsv, "hwmap=derive_device=qsv"),
            (HardwareAccelerationType::nvenc, "-init_hw_device cuda="),
        ] {
            let options = EncodingOptions {
                hardware_acceleration_type: accel,
                prefer_system_native_hw_decoder: true,
                enable_enhanced_nvdec_decoder: true,
                ..vaapi_options()
            };
            let (args, _) =
                build_accelerated_trickplay_args(&caps, &options, &job(&stream, "/tmp/o/%08d.jpg"))
                    .unwrap_or_else(|| panic!("{accel:?} has a ported chain"));
            assert!(args.contains(marker), "{accel:?}: {args}");
            assert!(args.contains("-threads 1 "), "{accel:?}: {args}");
        }

        // The four with no ported chain decline, whatever the keyframe gate
        // says about them -- that predicate is a faithful port, the chain is
        // the part that does not exist.
        for accel in [
            HardwareAccelerationType::amf,
            HardwareAccelerationType::videotoolbox,
            HardwareAccelerationType::rkmpp,
            HardwareAccelerationType::v4l2m2m,
        ] {
            let options = EncodingOptions {
                hardware_acceleration_type: accel,
                ..vaapi_options()
            };
            assert!(
                build_accelerated_trickplay_args(&caps, &options, &job(&stream, "/tmp/o/%08d.jpg"))
                    .is_none(),
                "{accel:?}"
            );
        }
    }

    #[test]
    fn the_corrected_height_is_what_the_hardware_scaler_is_told() {
        // The end of the correction: a 720x576 PAL source displayed 16:9 must
        // reach the scaler as 405 tall, not the 576 it is stored at.
        let caps = caps(Platform::Linux, true);
        let options = vaapi_options();
        let (args, _) = build_accelerated_trickplay_args(
            &caps,
            &options,
            &job(&stream(720, 576, Some("16:9")), "/tmp/o/%08d.jpg"),
        )
        .expect("vaapi chain");
        assert!(args.contains("scale_vaapi=w=320:h=180"), "{args}");
    }

    #[test]
    fn a_hardware_jpeg_quality_is_clamped_and_converted() {
        // Both ends of ffmpeg's 1-31 qscale range, mapped onto the 0-100 JPEG
        // quality VAAPI and QSV actually read. The step is C#'s *integer*
        // 100/30 == 3, so the worst quality is 10, not 0.
        assert_eq!(mjpeg_quality("mjpeg_vaapi", 1), ("-global_quality:v", 100));
        assert_eq!(mjpeg_quality("mjpeg_qsv", 31), ("-global_quality:v", 10));
        // Out-of-range input is clamped before conversion, as upstream does.
        assert_eq!(mjpeg_quality("mjpeg_vaapi", 0), ("-global_quality:v", 100));
        assert_eq!(mjpeg_quality("mjpeg_qsv", 99), ("-global_quality:v", 10));
        // A software encoder keeps ffmpeg's own scale, clamped.
        assert_eq!(mjpeg_quality("mjpeg", 4), ("-qscale:v", 4));
        assert_eq!(mjpeg_quality("mjpeg", 99), ("-qscale:v", 31));
        // The match is case-insensitive and substring, as upstream's is.
        assert_eq!(mjpeg_quality("MJPEG_VAAPI", 4), ("-global_quality:v", 91));
    }

    #[test]
    fn an_absent_render_node_still_builds_a_device() {
        // What a container with no `/dev/dri` produces, which is what CI is and
        // what an operator with a mistyped device path gets. The node is
        // resolved with `fs::metadata`, so it drops out of the device string
        // and ffmpeg is left to pick a device by driver -- a bare `va:`, not a
        // broken argument and not a crash. Untested until a CI run on a
        // GPU-less machine turned three green tests red.
        let caps = caps(Platform::Linux, true);
        let options = EncodingOptions {
            vaapi_device: Some("/dev/dri/renderD-absent".to_owned()),
            ..vaapi_options()
        };
        let (args, _) = build_accelerated_trickplay_args(
            &caps,
            &options,
            &job(&stream(1920, 1080, None), "/tmp/o/%08d.jpg"),
        )
        .expect("vaapi chain");
        assert!(
            args.contains("-init_hw_device vaapi=va:,driver=iHD"),
            "{args}"
        );
    }

    #[test]
    fn the_libva_driver_override_travels_with_the_arguments() {
        // i965 ranks below iHD in libva's own lookup, so it has to be named in
        // the child's environment or libva loads the other one. An ffmpeg
        // spawned with the arguments but not the environment silently runs on
        // the wrong driver -- which is why the builder returns both.
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .encoders(["mjpeg_vaapi", "mjpeg"])
            .hwaccels(["vaapi"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .vaapi_driver(false, false, true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let (_, env) = build_accelerated_trickplay_args(
            &caps,
            &vaapi_options(),
            &job(&stream(1920, 1080, None), "/tmp/o/%08d.jpg"),
        )
        .expect("vaapi chain");
        assert_eq!(
            env,
            vec![
                ("LIBVA_DRIVER_NAME".to_owned(), "i965".to_owned()),
                ("LIBVA_DRIVER_NAME_JELLYFIN".to_owned(), "i965".to_owned()),
            ]
        );
    }

    #[test]
    fn a_software_decoder_gets_no_noautoscale() {
        // Reachable, and the branch that regresses silently if the gate is
        // inverted: VAAPI with no codec ticked for hardware *decoding* still
        // gives a hardware MJPEG encoder, so the chain is built but there is
        // no hardware decoder for ffmpeg to insert a scaler behind.
        let caps = caps(Platform::Linux, true);
        let options = EncodingOptions {
            hardware_decoding_codecs: Vec::new(),
            ..vaapi_options()
        };
        let (args, _) = build_accelerated_trickplay_args(
            &caps,
            &options,
            &job(&stream(1920, 1080, None), "/tmp/o/%08d.jpg"),
        )
        .expect("the encoder is still hardware");
        assert!(args.contains("-c:v mjpeg_vaapi"), "{args}");
        assert!(!args.contains("-noautoscale"), "{args}");
    }

    #[test]
    fn a_zero_width_does_not_reach_the_scaler() {
        let caps = caps(Platform::Linux, true);
        let options = vaapi_options();
        let stream = stream(1920, 1080, None);
        let mut zero_width = job(&stream, "/tmp/o/%08d.jpg");
        zero_width.max_width = 0;
        assert!(build_accelerated_trickplay_args(&caps, &options, &zero_width).is_none());
    }

    #[test]
    fn a_zero_interval_does_not_produce_an_infinite_frame_rate() {
        let caps = caps(Platform::Linux, true);
        let options = vaapi_options();
        let stream = stream(1920, 1080, None);
        let mut zero = job(&stream, "/tmp/o/%08d.jpg");
        zero.interval_ms = 0;
        assert!(build_accelerated_trickplay_args(&caps, &options, &zero).is_none());
    }

    #[test]
    fn an_accelerator_that_cannot_skip_to_keyframes_stays_on_the_cpu() {
        // Decoding EVERY frame on the GPU is slower than decoding keyframes on
        // the CPU, so upstream declines hardware entirely rather than take it.
        let caps = caps(Platform::Linux, true);
        let nvenc_without_enhanced = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
            enable_enhanced_nvdec_decoder: false,
            ..vaapi_options()
        };
        assert!(!supports_keyframe_only_decode(
            &caps,
            &nvenc_without_enhanced
        ));
        assert!(
            build_accelerated_trickplay_args(
                &caps,
                &nvenc_without_enhanced,
                &job(&stream(1920, 1080, None), "/tmp/o/%08d.jpg")
            )
            .is_none()
        );

        // ...and with the enhanced decoder it does.
        let nvenc = EncodingOptions {
            enable_enhanced_nvdec_decoder: true,
            ..nvenc_without_enhanced
        };
        assert!(supports_keyframe_only_decode(&caps, &nvenc));
    }

    #[test]
    fn no_accelerator_means_no_accelerated_args() {
        let caps = caps(Platform::Linux, true);
        let options = EncodingOptions::default();
        assert!(
            build_accelerated_trickplay_args(
                &caps,
                &options,
                &job(&stream(1920, 1080, None), "/tmp/o/%08d.jpg")
            )
            .is_none()
        );
    }

    #[test]
    fn an_anamorphic_source_is_unstretched_before_a_hardware_scaler_sees_it() {
        // A hardware scaler takes fixed pixel sizes, so a source stored
        // stretched has to be corrected here — the software path avoids this by
        // expressing itself in `dar` terms instead.
        //
        // 720x576 stored, displayed 16:9: the real height is 720*9/16 = 405.
        assert_eq!(
            display_corrected_height(&stream(720, 576, Some("16:9"))),
            Some(405)
        );
        // Already square: left alone.
        assert_eq!(
            display_corrected_height(&stream(1920, 1080, Some("16:9"))),
            Some(1080)
        );
        // No aspect ratio to correct against.
        assert_eq!(display_corrected_height(&stream(720, 576, None)), None);
        // Nonsense ratio: no correction rather than a wrong one.
        assert_eq!(
            display_corrected_height(&stream(720, 576, Some("wide"))),
            None
        );
        // ffprobe writes `0:1` when it could not work the aspect out, and that
        // clears the "already square" guard (720*1 - 576*0 == 720). Upstream
        // divides by zero here and throws on the cast; a saturating Rust cast
        // would hand a hardware scaler a height of i32::MAX instead.
        assert_eq!(
            display_corrected_height(&stream(720, 576, Some("0:1"))),
            Some(576)
        );
        // `Convert.ToInt32` is banker's rounding: 705 wide at 2:1 is exactly
        // 352.5, which C# resolves down to 352 and `f64::round` would take up.
        assert_eq!(
            display_corrected_height(&stream(705, 400, Some("2:1"))),
            Some(352)
        );
    }

    #[test]
    fn the_keyframe_gate_follows_each_accelerators_own_switch() {
        let linux = caps(Platform::Linux, true);
        let windows = caps(Platform::Windows, true);
        let with = |accel, f: &dyn Fn(&mut EncodingOptions)| {
            let mut o = EncodingOptions {
                hardware_acceleration_type: accel,
                ..EncodingOptions::default()
            };
            f(&mut o);
            o
        };
        // AMF only on Windows, whatever the options say.
        let amf = with(HardwareAccelerationType::amf, &|_| {});
        assert!(supports_keyframe_only_decode(&windows, &amf));
        assert!(!supports_keyframe_only_decode(&linux, &amf));
        // QSV needs the native decoder preference.
        let qsv_native = with(HardwareAccelerationType::qsv, &|o| {
            o.prefer_system_native_hw_decoder = true;
        });
        let qsv_not = with(HardwareAccelerationType::qsv, &|o| {
            o.prefer_system_native_hw_decoder = false;
        });
        assert!(supports_keyframe_only_decode(&linux, &qsv_native));
        assert!(!supports_keyframe_only_decode(&linux, &qsv_not));
        // These three need nothing extra.
        for accel in [
            HardwareAccelerationType::vaapi,
            HardwareAccelerationType::videotoolbox,
            HardwareAccelerationType::rkmpp,
        ] {
            assert!(
                supports_keyframe_only_decode(&linux, &with(accel, &|_| {})),
                "{accel:?}"
            );
        }
    }
}
