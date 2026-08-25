//! Which decoder ffmpeg should use, and how the frames should leave it.
//!
//! Port of C# `EncodingHelper`'s decoder-selection block (10.11.z lines
//! 6304–7114): `GetVideoColorBitDepth`, `GetHardwareVideoDecoder`,
//! `GetHwDecoderName`, `GetHwaccelType`, and the six per-vendor selectors.
//!
//! Two different things come out of here and are concatenated:
//!
//! - **`-hwaccel <type>`**, which asks ffmpeg to decode on the GPU, optionally
//!   with `-hwaccel_output_format` so the frames stay in GPU memory for the
//!   filter chain instead of being copied back to the CPU.
//! - **`-c:v <name>`**, a *named* hardware decoder (`h264_qsv`, `hevc_cuvid`).
//!   VAAPI and AMF have none — they are hwaccel-only — and several are actively
//!   suppressed by configuration, which is why [`hw_decoder_name`] returns
//!   `None` far more often than it returns a name.
//!
//! Every returned string begins with a space and is concatenated by the caller,
//! matching how the C# builds it.

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::{HardwareAccelerationType, VideoType};
use ferrofin_model::entities_media::MediaStream;

use super::capabilities::FfmpegCapabilities;
use super::support::{
    is_cuda_full_supported, is_opencl_full_supported, is_rkmpp_full_supported,
    is_vaapi_full_supported, is_vaapi_supported, is_videotoolbox_full_supported,
};
use super::versions::{
    MIN_FFMPEG_DISPLAY_ROTATION_OPTION, MIN_FFMPEG_HWA_UNSAFE_OUTPUT, MIN_FFMPEG_IMPLICIT_HWACCEL,
    MIN_FFMPEG_WORKING_VT_HW_SURFACE,
};
use crate::encoder::FfmpegVersion;

/// The macOS release that gave Apple Silicon VideoToolbox an H.264 Hi10P
/// decoder. Port of the inline `new Version(14, 6)` in
/// `GetHardwareVideoDecoder`.
pub const MIN_MACOS_VT_H264_HI10P: FfmpegVersion = FfmpegVersion::new(14, 6);

/// The client-requested output size, as the RKMPP scale-ratio check reads it.
///
/// Port of the four `state.BaseRequest.{Width,Height,MaxWidth,MaxHeight}`
/// values `GetRkmppVidDecoder` passes to `IsScaleRatioSupported`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestedSize {
    /// Exact output width, if the client asked for one.
    pub width: Option<i32>,
    /// Exact output height, if the client asked for one.
    pub height: Option<i32>,
    /// Upper bound on output width.
    pub max_width: Option<i32>,
    /// Upper bound on output height.
    pub max_height: Option<i32>,
}

/// Everything the decoder selection reads about a job.
///
/// C# reaches through `EncodingJobInfo` for these; gathering them keeps the
/// selectors pure functions of their inputs and keeps every signature within
/// clippy's argument limit.
#[derive(Debug, Clone, Copy)]
pub struct DecodeContext<'a> {
    /// What the running ffmpeg and machine can do.
    pub caps: &'a FfmpegCapabilities,
    /// The persisted encoding options.
    pub options: &'a EncodingOptions,
    /// The source video stream.
    pub video_stream: Option<&'a MediaStream>,
    /// Whether the source is a plain file or a disc/folder rip.
    pub video_type: Option<VideoType>,
    /// The negotiated output codec; `copy` disables hardware decoding.
    pub output_video_codec: Option<&'a str>,
    /// The client-requested output size (RKMPP only).
    pub requested: RequestedSize,
}

/// The source's colour bit depth. Port of `GetVideoColorBitDepth`.
///
/// The stream's own `BitDepth` wins; otherwise it is inferred from the pixel
/// format, and anything unrecognised is assumed 8-bit. `0` means there is no
/// video stream at all.
#[must_use]
pub fn video_color_bit_depth(video_stream: Option<&MediaStream>) -> i32 {
    let Some(stream) = video_stream else {
        return 0;
    };
    if let Some(depth) = stream.bit_depth {
        return depth;
    }
    match stream.pixel_format.as_deref() {
        Some(f) if eq_any(f, &["yuv420p", "yuvj420p", "yuv422p", "yuv444p"]) => 8,
        Some(f) if eq_any(f, &["yuv420p10le", "yuv422p10le", "yuv444p10le"]) => 10,
        Some(f) if eq_any(f, &["yuv420p12le", "yuv422p12le", "yuv444p12le"]) => 12,
        _ => 8,
    }
}

/// Whether the source is HEVC Range Extensions. Port of `IsVideoStreamHevcRext`.
///
/// RExt covers the 4:2:2 / 4:4:4 / 12-bit HEVC profiles, which most hardware
/// decodes only on recent Intel parts — hence the separate configuration
/// switches and the iHD-only VAAPI gate.
#[must_use]
pub fn is_video_stream_hevc_rext(video_stream: Option<&MediaStream>) -> bool {
    let Some(stream) = video_stream else {
        return false;
    };
    if !stream.codec.as_deref().is_some_and(|c| eq(c, "hevc")) {
        return false;
    }
    stream.profile.as_deref().is_some_and(|p| eq(p, "Rext"))
        || stream.pixel_format.as_deref().is_some_and(|f| {
            eq_any(
                f,
                &[
                    "yuv420p12le",
                    "yuv422p",
                    "yuv422p10le",
                    "yuv422p12le",
                    "yuv444p",
                    "yuv444p10le",
                    "yuv444p12le",
                ],
            )
        })
}

/// The ` -c:v <prefix>_<suffix>` argument for a named hardware decoder, or
/// `None` when it must not be used. Port of `GetHwDecoderName`.
///
/// Returns `None` far more often than a name, for five separate reasons: the
/// build lacks the decoder; the operator did not enable hardware decoding for
/// that codec; a 10-bit HEVC/VP9 source with the matching depth switch off;
/// `cuvid` when the enhanced NVDEC path is on (which uses `-hwaccel cuda`
/// instead); `qsv` when the system-native decoder is preferred (ditto,
/// `-hwaccel vaapi`/`d3d11va`); and `rkmpp` always, which is hwaccel-only.
#[must_use]
pub fn hw_decoder_name(
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    decoder_prefix: &str,
    decoder_suffix: &str,
    video_codec: &str,
    bit_depth: i32,
) -> Option<String> {
    if decoder_prefix.is_empty() || decoder_suffix.is_empty() {
        return None;
    }
    let decoder_name = format!("{decoder_prefix}_{decoder_suffix}");
    let is_codec_available =
        caps.supports_decoder(&decoder_name) && decoding_enabled_for(options, video_codec);

    // VideoToolbox decoders fall back to software internally, so its 10-bit
    // switches are not consulted.
    if bit_depth == 10
        && is_codec_available
        && options.hardware_acceleration_type != HardwareAccelerationType::videotoolbox
    {
        if eq(video_codec, "hevc")
            && decoding_enabled_for(options, "hevc")
            && !options.enable_decoding_color_depth10_hevc
        {
            return None;
        }
        if eq(video_codec, "vp9")
            && decoding_enabled_for(options, "vp9")
            && !options.enable_decoding_color_depth10_vp9
        {
            return None;
        }
    }

    if eq(decoder_suffix, "cuvid") && options.enable_enhanced_nvdec_decoder {
        return None;
    }
    if eq(decoder_suffix, "qsv") && options.prefer_system_native_hw_decoder {
        return None;
    }
    if eq(decoder_suffix, "rkmpp") {
        return None;
    }

    is_codec_available.then(|| format!(" -c:v {decoder_name}"))
}

/// The four conditional flag fragments `GetHwaccelType` sprinkles through its
/// return strings, resolved once.
#[derive(Debug, Clone, Copy)]
struct HwaccelFlags {
    /// ` -c:v av1` — below ffmpeg 6.0 `-hwaccel` alone leaves AV1 to libdav1d,
    /// so the decoder must be named to engage the accelerator.
    av1: &'static str,
    /// ` -hwaccel_flags +allow_profile_mismatch` — d3d11va and vaapi decode
    /// H.264 baseline fine despite the profile not matching.
    profile_mismatch: &'static str,
    /// ` -hwaccel_flags +unsafe_output` — the filter chain already handles the
    /// copy, so nvdec need not do it again.
    unsafe_output: &'static str,
    /// ` -display_rotation 0` — a transposed fMP4 stream must not also carry
    /// rotation side data, or the client rotates it a second time.
    strip_rotation: &'static str,
}

impl HwaccelFlags {
    fn resolve(ctx: &DecodeContext<'_>, video_codec: &str) -> Self {
        let caps = ctx.caps;
        let is_av1 = !caps.ffmpeg_at_least(MIN_FFMPEG_IMPLICIT_HWACCEL) && eq(video_codec, "av1");
        let profile_mismatch = eq(video_codec, "h264")
            && ctx
                .video_stream
                .and_then(|s| s.profile.as_deref())
                .is_some_and(|p| eq(p, "baseline"));
        let strip_rotation = ctx
            .video_stream
            .and_then(|s| s.rotation)
            .is_some_and(|r| r != 0)
            && caps.ffmpeg_at_least(MIN_FFMPEG_DISPLAY_ROTATION_OPTION);
        Self {
            av1: if is_av1 { " -c:v av1" } else { "" },
            profile_mismatch: if profile_mismatch {
                " -hwaccel_flags +allow_profile_mismatch"
            } else {
                ""
            },
            unsafe_output: if caps.ffmpeg_at_least(MIN_FFMPEG_HWA_UNSAFE_OUTPUT) {
                " -hwaccel_flags +unsafe_output"
            } else {
                ""
            },
            strip_rotation: if strip_rotation {
                " -display_rotation 0"
            } else {
                ""
            },
        }
    }
}

/// Whether the configured colour-depth switches allow hardware decoding here.
///
/// Port of the HEVC/VP9 depth block inside `GetHwaccelType`. VideoToolbox is
/// exempt throughout: its decoders fall back to software internally rather than
/// failing, so upstream does not gate them.
fn colour_depth_gates_pass(
    ctx: &DecodeContext<'_>,
    video_codec: &str,
    bit_depth: i32,
    is_codec_available: bool,
) -> bool {
    let options = ctx.options;
    let hw_type = options.hardware_acceleration_type;
    if !is_codec_available || hw_type == HardwareAccelerationType::videotoolbox {
        return true;
    }

    if eq(video_codec, "hevc") && decoding_enabled_for(options, "hevc") {
        if is_video_stream_hevc_rext(ctx.video_stream) {
            if bit_depth <= 10 && !options.enable_decoding_color_depth10_hevc_rext {
                return false;
            }
            if bit_depth == 12 && !options.enable_decoding_color_depth12_hevc_rext {
                return false;
            }
            // Only the Intel iHD driver decodes HEVC RExt through VAAPI.
            if hw_type == HardwareAccelerationType::vaapi && !ctx.caps.is_vaapi_device_intel_ihd() {
                return false;
            }
        } else if bit_depth == 10 && !options.enable_decoding_color_depth10_hevc {
            return false;
        }
    }

    if eq(video_codec, "vp9")
        && decoding_enabled_for(options, "vp9")
        && bit_depth == 10
        && !options.enable_decoding_color_depth10_vp9
    {
        return false;
    }

    true
}

/// The ` -hwaccel …` argument for this job, or `None` when no hardware decode
/// applies. Port of `GetHwaccelType`.
///
/// `output_hw_surface` asks for `-hwaccel_output_format`, which keeps decoded
/// frames in GPU memory. The caller decides it from whether the vendor's filter
/// chain can consume those frames — without it the frames are copied back to
/// the CPU and the filters run in software.
#[must_use]
pub fn hwaccel_type(
    ctx: &DecodeContext<'_>,
    video_codec: &str,
    bit_depth: i32,
    output_hw_surface: bool,
) -> Option<String> {
    let caps = ctx.caps;
    let options = ctx.options;
    let platform = caps.platform();
    let source_codec = ctx.video_stream.and_then(|s| s.codec.as_deref());

    let is_d3d11_supported = platform.is_windows() && caps.supports_hwaccel("d3d11va");
    let is_vaapi_supported_here = platform.is_linux() && is_vaapi_supported(caps, source_codec);
    let is_cuda_supported =
        (platform.is_linux() || platform.is_windows()) && is_cuda_full_supported(caps);
    let is_qsv_supported =
        (platform.is_linux() || platform.is_windows()) && caps.supports_hwaccel("qsv");
    let is_videotoolbox_supported = platform.is_macos() && caps.supports_hwaccel("videotoolbox");
    let is_rkmpp_supported = platform.is_linux() && is_rkmpp_full_supported(caps);
    let is_codec_available = decoding_enabled_for(options, video_codec);
    let hw_type = options.hardware_acceleration_type;

    let flags = HwaccelFlags::resolve(ctx, video_codec);
    let (av1_arg, mismatch_arg, unsafe_output_arg, rotation_arg) = (
        flags.av1,
        flags.profile_mismatch,
        flags.unsafe_output,
        flags.strip_rotation,
    );

    if !colour_depth_gates_pass(ctx, video_codec, bit_depth, is_codec_available) {
        return None;
    }

    match hw_type {
        // Intel: either the native platform decoder (vaapi/d3d11va) or qsv.
        HardwareAccelerationType::qsv => {
            if options.prefer_system_native_hw_decoder {
                if is_vaapi_supported_here && is_codec_available {
                    return Some(format!(
                        " -hwaccel vaapi{}{mismatch_arg}{av1_arg}",
                        surface(output_hw_surface, "vaapi", rotation_arg)
                    ));
                }
                if is_d3d11_supported && is_codec_available {
                    return Some(format!(
                        " -hwaccel d3d11va{}{mismatch_arg} -threads 2{av1_arg}",
                        surface(output_hw_surface, "d3d11", rotation_arg)
                    ));
                }
            } else if is_qsv_supported && is_codec_available {
                return Some(format!(
                    " -hwaccel qsv{}",
                    surface(output_hw_surface, "qsv", rotation_arg)
                ));
            }
            None
        }
        HardwareAccelerationType::nvenc if is_cuda_supported && is_codec_available => {
            if options.enable_enhanced_nvdec_decoder {
                // nvdec implements no threading of its own, so pin it to one.
                Some(format!(
                    " -hwaccel cuda{}{unsafe_output_arg} -threads 1{av1_arg}",
                    surface(output_hw_surface, "cuda", rotation_arg)
                ))
            } else {
                // The cuvid decoder has no such threading issue.
                Some(format!(
                    " -hwaccel cuda{}",
                    surface(output_hw_surface, "cuda", rotation_arg)
                ))
            }
        }
        HardwareAccelerationType::amf if is_d3d11_supported && is_codec_available => Some(format!(
            " -hwaccel d3d11va{}{mismatch_arg} -threads 2{av1_arg}",
            surface(output_hw_surface, "d3d11", rotation_arg)
        )),
        HardwareAccelerationType::vaapi if is_vaapi_supported_here && is_codec_available => {
            Some(format!(
                " -hwaccel vaapi{}{mismatch_arg}{av1_arg}",
                surface(output_hw_surface, "vaapi", rotation_arg)
            ))
        }
        HardwareAccelerationType::videotoolbox
            if is_videotoolbox_supported && is_codec_available =>
        {
            // The only branch whose `-noautorotate` sits outside the surface
            // tail: VideoToolbox always emits it.
            let fmt = if output_hw_surface {
                " -hwaccel_output_format videotoolbox_vld"
            } else {
                ""
            };
            Some(format!(
                " -hwaccel videotoolbox{fmt} -noautorotate{rotation_arg}"
            ))
        }
        HardwareAccelerationType::rkmpp if is_rkmpp_supported && is_codec_available => {
            Some(format!(
                " -hwaccel rkmpp{}",
                surface(output_hw_surface, "drm_prime", rotation_arg)
            ))
        }
        _ => None,
    }
}

/// The `-hwaccel_output_format <fmt> -noautorotate` tail, present only when the
/// decoded frames are to stay in GPU memory.
fn surface(output_hw_surface: bool, format: &str, rotation_arg: &str) -> String {
    if output_hw_surface {
        format!(" -hwaccel_output_format {format} -noautorotate{rotation_arg}")
    } else {
        String::new()
    }
}

/// The complete hardware decode arguments for this job, or `None` to let ffmpeg
/// decide (which means software decoding). Port of `GetHardwareVideoDecoder`.
#[must_use]
pub fn hardware_video_decoder(ctx: &DecodeContext<'_>) -> Option<String> {
    let stream = ctx.video_stream?;

    // Hardware decoders handle both video files and disc/folder rips.
    if !matches!(
        ctx.video_type.unwrap_or(VideoType::VideoFile),
        VideoType::VideoFile | VideoType::Iso | VideoType::Dvd | VideoType::BluRay
    ) {
        return None;
    }
    if is_copy_codec(ctx.output_video_codec) {
        return None;
    }

    let codec = stream.codec.as_deref().filter(|c| !c.is_empty())?;
    let hw_type = ctx.options.hardware_acceleration_type;
    if hw_type == HardwareAccelerationType::none {
        return None;
    }

    let bit_depth = video_color_bit_depth_from(stream);

    // Only HEVC, VP9 and AV1 have 10-bit hardware decoders on most platforms.
    if bit_depth == 10 && !eq_any(codec, &["hevc", "h265", "vp9", "av1"]) {
        // RKMPP has an H.264 Hi10P decoder; so does VideoToolbox on Apple
        // Silicon from macOS 14.6.
        let mut has_hardware_hi10p = hw_type == HardwareAccelerationType::rkmpp;
        if hw_type == HardwareAccelerationType::videotoolbox
            && ctx.caps.is_arm64()
            && ctx
                .caps
                .os_version()
                .is_some_and(|v| v >= MIN_MACOS_VT_H264_HI10P)
        {
            has_hardware_hi10p = true;
        }
        if !has_hardware_hi10p && eq(codec, "h264") {
            return None;
        }
    }

    // H.264 Hi422P and Hi444PP can be carried in a 4:2:0 pixel format, so the
    // profile has to be checked rather than the format.
    if eq(codec, "h264")
        && stream
            .profile
            .as_deref()
            .is_some_and(|p| contains_ignore_case(p, "4:2:2") || contains_ignore_case(p, "4:4:4"))
        && !(hw_type == HardwareAccelerationType::videotoolbox && ctx.caps.is_arm64())
    {
        return None;
    }

    let decoder = match hw_type {
        HardwareAccelerationType::vaapi => vaapi_decoder(ctx, stream, bit_depth),
        HardwareAccelerationType::amf => amf_decoder(ctx, stream, bit_depth),
        HardwareAccelerationType::qsv => qsv_decoder(ctx, stream, bit_depth),
        HardwareAccelerationType::nvenc => nvdec_decoder(ctx, stream, bit_depth),
        HardwareAccelerationType::videotoolbox => videotoolbox_decoder(ctx, stream, bit_depth),
        HardwareAccelerationType::rkmpp => rkmpp_decoder(ctx, stream, bit_depth),
        HardwareAccelerationType::v4l2m2m | HardwareAccelerationType::none => None,
    };

    decoder.filter(|d| !d.is_empty())
}

/// The Quick Sync decoder selection. Port of `GetQsvHwVidDecoder`.
fn qsv_decoder(ctx: &DecodeContext<'_>, stream: &MediaStream, bit_depth: i32) -> Option<String> {
    let caps = ctx.caps;
    let platform = caps.platform();
    if !(platform.is_windows() || platform.is_linux())
        || ctx.options.hardware_acceleration_type != HardwareAccelerationType::qsv
    {
        return None;
    }

    let qsv_ocl = caps.supports_hwaccel("qsv") && is_opencl_full_supported(caps);
    let dx11_ocl = platform.is_windows() && caps.supports_hwaccel("d3d11va") && qsv_ocl;
    let vaapi_ocl =
        platform.is_linux() && is_vaapi_supported(caps, stream.codec.as_deref()) && qsv_ocl;
    let hw_surface = (dx11_ocl || vaapi_ocl) && caps.supports_filter("alphasrc");

    let fmt = stream.pixel_format.as_deref();
    let (prefix, codec) = match stream.codec.as_deref()? {
        c if is_8bit(fmt) && eq_any(c, &["avc", "h264"]) => ("h264", "h264"),
        c if is_8bit(fmt) && eq(c, "vc1") => ("vc1", "vc1"),
        c if is_8bit(fmt) && eq(c, "vp8") => ("vp8", "vp8"),
        c if is_8bit(fmt) && eq(c, "mpeg2video") => ("mpeg2", "mpeg2video"),
        c if is_8_10bit(fmt) && eq(c, "vp9") => ("vp9", "vp9"),
        c if is_8_10bit(fmt) && eq(c, "av1") => ("av1", "av1"),
        c if is_8_10_12bit_422_444(fmt) && eq_any(c, &["hevc", "h265"]) => ("hevc", "hevc"),
        _ => return None,
    };
    Some(join(
        hwaccel_type(ctx, codec, bit_depth, hw_surface),
        hw_decoder_name(caps, ctx.options, prefix, "qsv", codec, bit_depth),
    ))
}

/// The NVDEC / CUVID decoder selection. Port of `GetNvdecVidDecoder`.
fn nvdec_decoder(ctx: &DecodeContext<'_>, stream: &MediaStream, bit_depth: i32) -> Option<String> {
    let caps = ctx.caps;
    let platform = caps.platform();
    if !(platform.is_windows() || platform.is_linux())
        || ctx.options.hardware_acceleration_type != HardwareAccelerationType::nvenc
    {
        return None;
    }
    let hw_surface = is_cuda_full_supported(caps) && caps.supports_filter("alphasrc");

    let fmt = stream.pixel_format.as_deref();
    let (prefix, codec) = match stream.codec.as_deref()? {
        c if is_8bit(fmt) && eq_any(c, &["avc", "h264"]) => ("h264", "h264"),
        c if is_8bit(fmt) && eq(c, "mpeg2video") => ("mpeg2", "mpeg2video"),
        c if is_8bit(fmt) && eq(c, "vc1") => ("vc1", "vc1"),
        c if is_8bit(fmt) && eq(c, "mpeg4") => ("mpeg4", "mpeg4"),
        c if is_8bit(fmt) && eq(c, "vp8") => ("vp8", "vp8"),
        c if is_8_10bit(fmt) && eq(c, "vp9") => ("vp9", "vp9"),
        c if is_8_10bit(fmt) && eq(c, "av1") => ("av1", "av1"),
        c if is_8_10_12bit_444(fmt) && eq_any(c, &["hevc", "h265"]) => ("hevc", "hevc"),
        _ => return None,
    };
    Some(join(
        hwaccel_type(ctx, codec, bit_depth, hw_surface),
        hw_decoder_name(caps, ctx.options, prefix, "cuvid", codec, bit_depth),
    ))
}

/// The AMD AMF decoder selection. Port of `GetAmfVidDecoder`.
///
/// AMF has no ffmpeg decoders of its own, so this returns the `-hwaccel`
/// argument alone.
fn amf_decoder(ctx: &DecodeContext<'_>, stream: &MediaStream, bit_depth: i32) -> Option<String> {
    let caps = ctx.caps;
    if !caps.platform().is_windows()
        || ctx.options.hardware_acceleration_type != HardwareAccelerationType::amf
    {
        return None;
    }
    let hw_surface = caps.supports_hwaccel("d3d11va")
        && is_opencl_full_supported(caps)
        && caps.supports_filter("alphasrc");

    let fmt = stream.pixel_format.as_deref();
    let codec = match stream.codec.as_deref()? {
        c if is_8bit(fmt) && eq_any(c, &["avc", "h264"]) => "h264",
        c if is_8bit(fmt) && eq(c, "mpeg2video") => "mpeg2video",
        c if is_8bit(fmt) && eq(c, "vc1") => "vc1",
        c if is_8_10bit(fmt) && eq_any(c, &["hevc", "h265"]) => "hevc",
        c if is_8_10bit(fmt) && eq(c, "vp9") => "vp9",
        c if is_8_10bit(fmt) && eq(c, "av1") => "av1",
        _ => return None,
    };
    hwaccel_type(ctx, codec, bit_depth, hw_surface)
}

/// The VAAPI decoder selection. Port of `GetVaapiVidDecoder`.
///
/// Like AMF, hwaccel-only — there are no `*_vaapi` decoders.
fn vaapi_decoder(ctx: &DecodeContext<'_>, stream: &MediaStream, bit_depth: i32) -> Option<String> {
    let caps = ctx.caps;
    if !caps.platform().is_linux()
        || ctx.options.hardware_acceleration_type != HardwareAccelerationType::vaapi
    {
        return None;
    }
    let hw_surface = is_vaapi_supported(caps, stream.codec.as_deref())
        && is_vaapi_full_supported(caps)
        && is_opencl_full_supported(caps)
        && caps.supports_filter("alphasrc");

    let fmt = stream.pixel_format.as_deref();
    let codec = match stream.codec.as_deref()? {
        c if is_8bit(fmt) && eq_any(c, &["avc", "h264"]) => "h264",
        c if is_8bit(fmt) && eq(c, "mpeg2video") => "mpeg2video",
        c if is_8bit(fmt) && eq(c, "vc1") => "vc1",
        c if is_8bit(fmt) && eq(c, "vp8") => "vp8",
        c if is_8_10bit(fmt) && eq(c, "vp9") => "vp9",
        c if is_8_10bit(fmt) && eq(c, "av1") => "av1",
        c if is_8_10_12bit_422_444(fmt) && eq_any(c, &["hevc", "h265"]) => "hevc",
        _ => return None,
    };
    hwaccel_type(ctx, codec, bit_depth, hw_surface)
}

/// The VideoToolbox decoder selection. Port of `GetVideotoolboxVidDecoder`.
fn videotoolbox_decoder(
    ctx: &DecodeContext<'_>,
    stream: &MediaStream,
    bit_depth: i32,
) -> Option<String> {
    let caps = ctx.caps;
    if !caps.platform().is_macos()
        || ctx.options.hardware_acceleration_type != HardwareAccelerationType::videotoolbox
    {
        return None;
    }
    // VideoToolbox hardware surfaces only work from jellyfin-ffmpeg 7.0.1.
    let hw_surface = caps.ffmpeg_at_least(MIN_FFMPEG_WORKING_VT_HW_SURFACE)
        && is_videotoolbox_full_supported(caps);

    let fmt = stream.pixel_format.as_deref();
    let codec = match stream.codec.as_deref()? {
        c if is_8bit(fmt) && eq(c, "vp8") => "vp8",
        c if is_8_10bit(fmt) && eq_any(c, &["avc", "h264"]) => "h264",
        c if is_8_10bit(fmt) && eq(c, "vp9") => "vp9",
        c if is_8_10_12bit_422_444(fmt) && eq_any(c, &["hevc", "h265"]) => "hevc",
        c if is_8_10_12bit_422_444(fmt)
            && eq(c, "av1")
            // Upstream's AV1 format set is the 8/10-bit one plus yuv420p12le,
            // which the 12-bit set already contains — so the extra condition
            // only excludes the 4:2:2 and 4:4:4 formats.
            && is_av1_vt_format(fmt)
            && caps.is_videotoolbox_av1_decode() =>
        {
            "av1"
        }
        _ => return None,
    };
    hwaccel_type(ctx, codec, bit_depth, hw_surface)
}

/// The Rockchip MPP decoder selection. Port of `GetRkmppVidDecoder`.
fn rkmpp_decoder(ctx: &DecodeContext<'_>, stream: &MediaStream, bit_depth: i32) -> Option<String> {
    let caps = ctx.caps;
    if !caps.platform().is_linux()
        || ctx.options.hardware_acceleration_type != HardwareAccelerationType::rkmpp
    {
        return None;
    }

    let in_w = stream.width;
    let in_h = stream.height;
    let req = ctx.requested;

    // The RGA2e block scales between 1/16x and 16x; outside that, no hardware.
    if !is_scale_ratio_supported(in_w, in_h, req, 16.0) {
        return None;
    }

    let rkmpp_ocl = is_rkmpp_full_supported(caps) && is_opencl_full_supported(caps);
    let hw_surface = rkmpp_ocl && caps.supports_filter("alphasrc");
    // The RGA3 block, which AFBC needs, is limited to 1/8x..8x.
    let afbc_supported = hw_surface && is_scale_ratio_supported(in_w, in_h, req, 8.0);

    let fmt = stream.pixel_format.as_deref();
    let is_10bit = fmt.is_some_and(|f| eq(f, "yuv420p10le"));
    // nv15 and nv20 are bitstream-only formats, so 10-bit needs a hw surface.
    if is_10bit && !hw_surface {
        return None;
    }

    let (codec, allow_afbc) = match stream.codec.as_deref()? {
        c if is_8bit(fmt) && eq(c, "mpeg1video") => ("mpeg1video", false),
        c if is_8bit(fmt) && eq(c, "mpeg2video") => ("mpeg2video", false),
        c if is_8bit(fmt) && eq(c, "mpeg4") => ("mpeg4", false),
        c if is_8bit(fmt) && eq(c, "vp8") => ("vp8", false),
        c if is_8_10bit(fmt) && eq_any(c, &["avc", "h264"]) => ("h264", true),
        c if is_8_10bit(fmt) && eq_any(c, &["hevc", "h265"]) => ("hevc", true),
        c if is_8_10bit(fmt) && eq(c, "vp9") => ("vp9", true),
        // AV1 AFBC is broken on RK3588, so it is left off until fixed upstream.
        c if is_8_10bit(fmt) && eq(c, "av1") => ("av1", false),
        _ => return None,
    };

    let accel = hwaccel_type(ctx, codec, bit_depth, hw_surface)?;
    Some(if allow_afbc && afbc_supported && !accel.is_empty() {
        format!("{accel} -afbc rga")
    } else {
        accel
    })
}

/// The output size a hardware scaler would produce. Port of
/// `GetFixedOutputSize`.
///
/// Hardware output is capped at 4K, and both dimensions are rounded down to an
/// even number because every hardware scaler here requires it.
#[must_use]
pub fn fixed_output_size(
    video_width: Option<i32>,
    video_height: Option<i32>,
    requested: RequestedSize,
) -> (Option<i32>, Option<i32>) {
    if video_width.is_none() && requested.width.is_none() {
        return (None, None);
    }
    if video_height.is_none() && requested.height.is_none() {
        return (None, None);
    }
    let Some(input_width) = video_width.or(requested.width) else {
        return (None, None);
    };
    let Some(input_height) = video_height.or(requested.height) else {
        return (None, None);
    };
    let mut output_width = requested.width.unwrap_or(input_width);
    let mut output_height = requested.height.unwrap_or(input_height);

    // Never transcode above 4K on hardware.
    let maximum_width = requested.max_width.unwrap_or(output_width).min(4096);
    let maximum_height = requested.max_height.unwrap_or(output_height).min(4096);

    if output_width > maximum_width || output_height > maximum_height {
        let scale_w = f64::from(maximum_width) / f64::from(output_width);
        let scale_h = f64::from(maximum_height) / f64::from(output_height);
        let scale = scale_w.min(scale_h);
        output_width = maximum_width.min(round_to_i32(f64::from(output_width) * scale));
        output_height = maximum_height.min(round_to_i32(f64::from(output_height) * scale));
    }

    (Some(2 * (output_width / 2)), Some(2 * (output_height / 2)))
}

/// Whether a hardware scaler limited to `max_scale_ratio` can produce the
/// requested size. Port of `IsScaleRatioSupported`.
#[must_use]
pub fn is_scale_ratio_supported(
    video_width: Option<i32>,
    video_height: Option<i32>,
    requested: RequestedSize,
    max_scale_ratio: f64,
) -> bool {
    let (out_width, out_height) = fixed_output_size(video_width, video_height, requested);
    let (Some(video_width), Some(video_height), Some(out_width), Some(out_height)) =
        (video_width, video_height, out_width, out_height)
    else {
        return false;
    };
    if max_scale_ratio < 1.0 {
        return false;
    }
    let min_scale_ratio = 1.0 / max_scale_ratio;
    let ratio_w = f64::from(out_width) / f64::from(video_width);
    let ratio_h = f64::from(out_height) / f64::from(video_height);
    ratio_w >= min_scale_ratio
        && ratio_w <= max_scale_ratio
        && ratio_h >= min_scale_ratio
        && ratio_h <= max_scale_ratio
}

// ----- small shared helpers -------------------------------------------------

/// `Convert.ToInt32(double)`: round to nearest, ties to even.
///
/// .NET uses banker's rounding here, not truncation, so a scaled dimension of
/// `1279.5` becomes `1280` and `1278.5` becomes `1278`.
fn round_to_i32(value: f64) -> i32 {
    let rounded = round_half_to_even(value);
    if rounded >= f64::from(i32::MAX) {
        i32::MAX
    } else if rounded <= f64::from(i32::MIN) {
        i32::MIN
    } else {
        // The bounds check above makes this exact.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "clamped to the i32 range immediately above, and the value \
                      is integral after rounding"
        )]
        {
            rounded as i32
        }
    }
}

/// Round half to even, the rule `Math.Round`/`Convert.ToInt32` use.
///
/// Above the midpoint always rounds up; exactly on it goes to whichever of the
/// two neighbours is even, which is why `0.5` becomes `0` and `1.5` becomes `2`.
fn round_half_to_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    // `diff` is an exact binary fraction here, so the midpoint really is
    // representable and comparing to it is meaningful rather than approximate;
    // `total_cmp` says so without tripping the float-equality lint.
    let ordering = diff.total_cmp(&0.5);
    let floor_is_odd = (floor / 2.0).fract() != 0.0;
    if ordering.is_gt() || (ordering.is_eq() && floor_is_odd) {
        floor + 1.0
    } else {
        floor
    }
}

/// `state.OutputVideoCodec` is a stream copy.
fn is_copy_codec(codec: Option<&str>) -> bool {
    codec.is_some_and(|c| eq(c, "copy"))
}

/// Whether the operator enabled hardware decoding for `codec`. Port of
/// `options.HardwareDecodingCodecs.Contains(codec, OrdinalIgnoreCase)`.
fn decoding_enabled_for(options: &EncodingOptions, codec: &str) -> bool {
    options
        .hardware_decoding_codecs
        .iter()
        .any(|c| c.eq_ignore_ascii_case(codec))
}

/// Concatenates the hwaccel argument and the optional named decoder, the way
/// C# concatenates two possibly-null strings (a null contributes nothing).
fn join(accel: Option<String>, decoder: Option<String>) -> String {
    let mut out = accel.unwrap_or_default();
    if let Some(decoder) = decoder {
        out.push_str(&decoder);
    }
    out
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn eq_any(value: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|c| value.eq_ignore_ascii_case(c))
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// The bit depth of a stream that is known to exist.
fn video_color_bit_depth_from(stream: &MediaStream) -> i32 {
    video_color_bit_depth(Some(stream))
}

/// The 8-bit software formats every vendor accepts.
fn is_8bit(pixel_format: Option<&str>) -> bool {
    pixel_format.is_some_and(|f| eq_any(f, &["yuv420p", "yuvj420p"]))
}

/// The 8-bit formats plus 10-bit 4:2:0.
fn is_8_10bit(pixel_format: Option<&str>) -> bool {
    is_8bit(pixel_format) || pixel_format.is_some_and(|f| eq(f, "yuv420p10le"))
}

/// The QSV/VAAPI/VideoToolbox wide set: 8/10-bit plus 4:2:2, 4:4:4 and 12-bit.
fn is_8_10_12bit_422_444(pixel_format: Option<&str>) -> bool {
    is_8_10bit(pixel_format)
        || pixel_format.is_some_and(|f| {
            eq_any(
                f,
                &[
                    "yuv422p",
                    "yuv444p",
                    "yuv422p10le",
                    "yuv444p10le",
                    "yuv420p12le",
                    "yuv422p12le",
                    "yuv444p12le",
                ],
            )
        })
}

/// The NVDEC wide set, which is 4:4:4 only — no 4:2:2.
fn is_8_10_12bit_444(pixel_format: Option<&str>) -> bool {
    is_8_10bit(pixel_format)
        || pixel_format
            .is_some_and(|f| eq_any(f, &["yuv444p", "yuv444p10le", "yuv420p12le", "yuv444p12le"]))
}

/// The formats VideoToolbox will decode AV1 in: the 8/10-bit set plus
/// `yuv420p12le`. Port of `isAv1SupportedSwFormatsVt`.
fn is_av1_vt_format(pixel_format: Option<&str>) -> bool {
    is_8_10bit(pixel_format) || pixel_format.is_some_and(|f| eq(f, "yuv420p12le"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::REQUIRED_DECODERS;
    use crate::encoding_helper::hw::capabilities::{FilterOption, Platform};
    use rstest::rstest;

    // Every expectation is hand-derived from the C# at EncodingHelper.cs
    // (10.11.z lines 6304-7114). Upstream ships no tests for any of it.

    /// A capability set with everything, on `platform`.
    fn caps_for(platform: Platform) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(platform)
            .hwaccels([
                "drm",
                "vaapi",
                "rkmpp",
                "opencl",
                "cuda",
                "vulkan",
                "videotoolbox",
                "qsv",
                "d3d11va",
            ])
            .filters(crate::encoder::REQUIRED_FILTERS)
            .decoders(REQUIRED_DECODERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build()
    }

    /// Encoding options with `hw_type` selected and every codec enabled for
    /// hardware decoding (the shipped default is only `["h264", "vc1"]`).
    fn options_for(hw_type: HardwareAccelerationType) -> EncodingOptions {
        let mut options = EncodingOptions {
            hardware_acceleration_type: hw_type,
            ..EncodingOptions::default()
        };
        options.hardware_decoding_codecs = [
            "h264",
            "hevc",
            "vc1",
            "vp8",
            "vp9",
            "av1",
            "mpeg2video",
            "mpeg4",
            "mpeg1video",
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
        options
    }

    fn stream(codec: &str, pixel_format: &str) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            pixel_format: Some(pixel_format.to_owned()),
            ..MediaStream::default()
        }
    }

    fn ctx<'a>(
        caps: &'a FfmpegCapabilities,
        options: &'a EncodingOptions,
        video_stream: &'a MediaStream,
    ) -> DecodeContext<'a> {
        DecodeContext {
            caps,
            options,
            video_stream: Some(video_stream),
            video_type: None,
            output_video_codec: Some("h264"),
            requested: RequestedSize::default(),
        }
    }

    // ----- bit depth ---------------------------------------------------------

    #[rstest]
    #[case(None, "yuv420p", 8)]
    #[case(None, "yuvj420p", 8)]
    #[case(None, "yuv422p", 8)]
    #[case(None, "yuv444p", 8)]
    #[case(None, "yuv420p10le", 10)]
    #[case(None, "yuv422p10le", 10)]
    #[case(None, "yuv444p10le", 10)]
    #[case(None, "yuv420p12le", 12)]
    #[case(None, "yuv422p12le", 12)]
    #[case(None, "yuv444p12le", 12)]
    // Anything unrecognised is assumed 8-bit rather than rejected.
    #[case(None, "rgb24", 8)]
    // A declared bit depth always wins over the pixel format.
    #[case(Some(12), "yuv420p", 12)]
    fn bit_depth_comes_from_the_stream_then_the_pixel_format(
        #[case] declared: Option<i32>,
        #[case] pixel_format: &str,
        #[case] expected: i32,
    ) {
        let mut s = stream("hevc", pixel_format);
        s.bit_depth = declared;
        assert_eq!(video_color_bit_depth(Some(&s)), expected);
    }

    #[test]
    fn no_video_stream_has_no_bit_depth() {
        assert_eq!(video_color_bit_depth(None), 0);
    }

    // ----- HEVC RExt ---------------------------------------------------------

    #[rstest]
    #[case("hevc", "Rext", "yuv420p", true)]
    #[case("hevc", "Main", "yuv422p", true)]
    #[case("hevc", "Main", "yuv444p10le", true)]
    #[case("hevc", "Main", "yuv420p12le", true)]
    // 4:2:0 8/10-bit HEVC is plain Main/Main10, not RExt.
    #[case("hevc", "Main", "yuv420p", false)]
    #[case("hevc", "Main10", "yuv420p10le", false)]
    // The codec has to be HEVC; an identical H.264 stream is not RExt.
    #[case("h264", "Rext", "yuv422p", false)]
    fn hevc_rext_is_the_wide_profile_set(
        #[case] codec: &str,
        #[case] profile: &str,
        #[case] pixel_format: &str,
        #[case] expected: bool,
    ) {
        let mut s = stream(codec, pixel_format);
        s.profile = Some(profile.to_owned());
        assert_eq!(is_video_stream_hevc_rext(Some(&s)), expected);
        assert!(!is_video_stream_hevc_rext(None));
    }

    // ----- named hardware decoders ------------------------------------------

    #[test]
    fn a_named_decoder_needs_the_build_and_the_operators_permission() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::qsv);
        // `prefer_system_native_hw_decoder` defaults true and suppresses qsv
        // names, so use nvenc's cuvid with the enhanced path off.
        let mut options = options;
        options.hardware_acceleration_type = HardwareAccelerationType::nvenc;
        options.enable_enhanced_nvdec_decoder = false;
        assert_eq!(
            hw_decoder_name(&caps, &options, "h264", "cuvid", "h264", 8).as_deref(),
            Some(" -c:v h264_cuvid")
        );

        // A build without the decoder.
        let bare = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .build();
        assert_eq!(
            hw_decoder_name(&bare, &options, "h264", "cuvid", "h264", 8),
            None
        );

        // The operator did not enable hardware decoding for this codec.
        let mut narrow = options.clone();
        narrow.hardware_decoding_codecs = vec!["vc1".to_owned()];
        assert_eq!(
            hw_decoder_name(&caps, &narrow, "h264", "cuvid", "h264", 8),
            None
        );

        // Empty prefix or suffix is not a decoder name.
        assert_eq!(
            hw_decoder_name(&caps, &options, "", "cuvid", "h264", 8),
            None
        );
        assert_eq!(
            hw_decoder_name(&caps, &options, "h264", "", "h264", 8),
            None
        );
    }

    #[test]
    fn three_suffixes_are_suppressed_outright() {
        let caps = caps_for(Platform::Linux);

        // cuvid, when the enhanced NVDEC path is on (the default) — that path
        // uses `-hwaccel cuda` instead of a named decoder.
        let mut options = options_for(HardwareAccelerationType::nvenc);
        options.enable_enhanced_nvdec_decoder = true;
        assert_eq!(
            hw_decoder_name(&caps, &options, "h264", "cuvid", "h264", 8),
            None
        );

        // qsv, when the system-native decoder is preferred (also the default).
        let mut options = options_for(HardwareAccelerationType::qsv);
        options.prefer_system_native_hw_decoder = true;
        assert_eq!(
            hw_decoder_name(&caps, &options, "h264", "qsv", "h264", 8),
            None
        );
        options.prefer_system_native_hw_decoder = false;
        assert_eq!(
            hw_decoder_name(&caps, &options, "h264", "qsv", "h264", 8).as_deref(),
            Some(" -c:v h264_qsv")
        );

        // rkmpp, always — it is hwaccel-only.
        let mut options = options_for(HardwareAccelerationType::rkmpp);
        options.prefer_system_native_hw_decoder = false;
        assert_eq!(
            hw_decoder_name(&caps, &options, "h264", "rkmpp", "h264", 8),
            None
        );
    }

    #[rstest]
    // The 10-bit switches gate the named decoder for HEVC and VP9 only...
    #[case("hevc", "cuvid", 10, false, None)]
    #[case("hevc", "cuvid", 10, true, Some(" -c:v hevc_cuvid"))]
    #[case("vp9", "cuvid", 10, false, None)]
    #[case("vp9", "cuvid", 10, true, Some(" -c:v vp9_cuvid"))]
    // ...and only at 10 bits; an 8-bit stream is unaffected by them.
    #[case("hevc", "cuvid", 8, false, Some(" -c:v hevc_cuvid"))]
    fn the_ten_bit_switches_gate_hevc_and_vp9(
        #[case] codec: &str,
        #[case] suffix: &str,
        #[case] bit_depth: i32,
        #[case] enabled: bool,
        #[case] expected: Option<&str>,
    ) {
        let caps = caps_for(Platform::Linux);
        let mut options = options_for(HardwareAccelerationType::nvenc);
        options.enable_enhanced_nvdec_decoder = false;
        options.enable_decoding_color_depth10_hevc = enabled;
        options.enable_decoding_color_depth10_vp9 = enabled;
        assert_eq!(
            hw_decoder_name(&caps, &options, codec, suffix, codec, bit_depth).as_deref(),
            expected
        );
    }

    // ----- the -hwaccel argument --------------------------------------------

    #[test]
    fn nvenc_emits_cuda_with_the_enhanced_nvdec_shape() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        let ctx = ctx(&caps, &options, &s);
        // Enhanced NVDEC: hw surface, unsafe output, single-threaded.
        assert_eq!(
            hwaccel_type(&ctx, "h264", 8, true).as_deref(),
            Some(
                " -hwaccel cuda -hwaccel_output_format cuda -noautorotate \
                 -hwaccel_flags +unsafe_output -threads 1"
            )
        );
        // Without a hardware surface the frames come back to the CPU.
        assert_eq!(
            hwaccel_type(&ctx, "h264", 8, false).as_deref(),
            Some(" -hwaccel cuda -hwaccel_flags +unsafe_output -threads 1")
        );
    }

    #[test]
    fn nvenc_with_cuvid_omits_the_threading_and_copy_flags() {
        let caps = caps_for(Platform::Linux);
        let mut options = options_for(HardwareAccelerationType::nvenc);
        options.enable_enhanced_nvdec_decoder = false;
        let s = stream("h264", "yuv420p");
        let ctx = ctx(&caps, &options, &s);
        assert_eq!(
            hwaccel_type(&ctx, "h264", 8, true).as_deref(),
            Some(" -hwaccel cuda -hwaccel_output_format cuda -noautorotate")
        );
    }

    #[test]
    fn qsv_prefers_the_platforms_native_decoder_by_default() {
        let options = options_for(HardwareAccelerationType::qsv);
        let s = stream("h264", "yuv420p");

        // Linux: vaapi.
        let linux = caps_for(Platform::Linux);
        assert_eq!(
            hwaccel_type(&ctx(&linux, &options, &s), "h264", 8, true).as_deref(),
            Some(" -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate")
        );

        // Windows: d3d11va, with its two threads.
        let windows = caps_for(Platform::Windows);
        assert_eq!(
            hwaccel_type(&ctx(&windows, &options, &s), "h264", 8, true).as_deref(),
            Some(" -hwaccel d3d11va -hwaccel_output_format d3d11 -noautorotate -threads 2")
        );

        // With the preference off, qsv itself.
        let mut native_off = options.clone();
        native_off.prefer_system_native_hw_decoder = false;
        assert_eq!(
            hwaccel_type(&ctx(&linux, &native_off, &s), "h264", 8, true).as_deref(),
            Some(" -hwaccel qsv -hwaccel_output_format qsv -noautorotate")
        );
    }

    #[test]
    fn videotoolbox_always_emits_noautorotate_outside_the_surface_tail() {
        let caps = caps_for(Platform::MacOs);
        let options = options_for(HardwareAccelerationType::videotoolbox);
        let s = stream("hevc", "yuv420p10le");
        let ctx = ctx(&caps, &options, &s);
        assert_eq!(
            hwaccel_type(&ctx, "hevc", 10, true).as_deref(),
            Some(" -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld -noautorotate")
        );
        // The one backend that keeps `-noautorotate` without a hw surface.
        assert_eq!(
            hwaccel_type(&ctx, "hevc", 10, false).as_deref(),
            Some(" -hwaccel videotoolbox -noautorotate")
        );
    }

    #[test]
    fn a_baseline_h264_profile_allows_the_mismatch_on_vaapi_and_d3d11() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::vaapi);
        let mut s = stream("h264", "yuv420p");
        s.profile = Some("baseline".to_owned());
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, false).as_deref(),
            Some(" -hwaccel vaapi -hwaccel_flags +allow_profile_mismatch")
        );
    }

    #[test]
    fn a_rotated_stream_strips_its_rotation_side_data() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::vaapi);
        let mut s = stream("h264", "yuv420p");
        s.rotation = Some(90);
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, true).as_deref(),
            Some(" -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate -display_rotation 0")
        );
        // No rotation, no argument.
        let s = stream("h264", "yuv420p");
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, true).as_deref(),
            Some(" -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate")
        );
    }

    #[test]
    fn an_old_ffmpeg_must_name_the_av1_decoder_explicitly() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi"])
            .ffmpeg_version(FfmpegVersion::new(5, 1))
            .build();
        let options = options_for(HardwareAccelerationType::vaapi);
        let s = stream("av1", "yuv420p");
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "av1", 8, false).as_deref(),
            Some(" -hwaccel vaapi -c:v av1")
        );
        // From 6.0 the hwaccel implies it.
        let caps = caps_for(Platform::Linux);
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "av1", 8, false).as_deref(),
            Some(" -hwaccel vaapi")
        );
    }

    #[test]
    fn hevc_rext_needs_its_own_switches_and_the_ihd_driver() {
        let caps_builder = || {
            FfmpegCapabilities::builder()
                .platform(Platform::Linux)
                .hwaccels(["vaapi", "drm", "opencl"])
                .filters(crate::encoder::REQUIRED_FILTERS)
                .all_filter_options(true)
                .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
        };
        let options = options_for(HardwareAccelerationType::vaapi);
        // 4:2:2 HEVC is RExt, and the RExt switch is off by default.
        let s = stream("hevc", "yuv422p10le");
        let ihd = caps_builder().vaapi_driver(false, true, false).build();
        assert_eq!(
            hwaccel_type(&ctx(&ihd, &options, &s), "hevc", 10, false),
            None
        );

        let mut rext_on = options.clone();
        rext_on.enable_decoding_color_depth10_hevc_rext = true;
        assert!(hwaccel_type(&ctx(&ihd, &rext_on, &s), "hevc", 10, false).is_some());

        // Same settings on an i965 device: VAAPI RExt is iHD-only.
        let i965 = caps_builder().vaapi_driver(false, false, true).build();
        assert_eq!(
            hwaccel_type(&ctx(&i965, &rext_on, &s), "hevc", 10, false),
            None
        );

        // 12-bit has its own switch.
        let s12 = stream("hevc", "yuv420p12le");
        assert_eq!(
            hwaccel_type(&ctx(&ihd, &rext_on, &s12), "hevc", 12, false),
            None
        );
        let mut rext12 = rext_on.clone();
        rext12.enable_decoding_color_depth12_hevc_rext = true;
        assert!(hwaccel_type(&ctx(&ihd, &rext12, &s12), "hevc", 12, false).is_some());
    }

    #[test]
    fn the_wrong_platform_offers_no_hardware() {
        let options = options_for(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        for platform in [Platform::Windows, Platform::MacOs, Platform::Other] {
            let caps = caps_for(platform);
            assert_eq!(
                hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, false),
                None,
                "VAAPI must be Linux-only, but {platform:?} accepted it"
            );
        }
        // ...and VideoToolbox is macOS-only.
        let options = options_for(HardwareAccelerationType::videotoolbox);
        for platform in [Platform::Linux, Platform::Windows, Platform::Other] {
            let caps = caps_for(platform);
            assert_eq!(
                hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, false),
                None,
                "VideoToolbox must be macOS-only, but {platform:?} accepted it"
            );
        }
    }

    // ----- the whole decoder selection --------------------------------------

    #[test]
    fn a_stream_copy_or_missing_stream_decodes_nothing() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        let mut c = ctx(&caps, &options, &s);
        c.output_video_codec = Some("copy");
        assert_eq!(hardware_video_decoder(&c), None);

        let mut c = ctx(&caps, &options, &s);
        c.video_stream = None;
        assert_eq!(hardware_video_decoder(&c), None);

        // ...and so does an unset accelerator.
        let none = options_for(HardwareAccelerationType::none);
        assert_eq!(hardware_video_decoder(&ctx(&caps, &none, &s)), None);
    }

    #[test]
    fn ten_bit_h264_has_no_hardware_decoder_on_most_platforms() {
        // Only HEVC/VP9/AV1 have 10-bit hardware decode; H.264 Hi10P is the
        // exception RKMPP and Apple Silicon carve out.
        let s = stream("h264", "yuv420p10le");
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::nvenc);
        assert_eq!(hardware_video_decoder(&ctx(&caps, &options, &s)), None);

        // RKMPP does have it — but only once the RGA scale-ratio check can run,
        // which needs the source dimensions.
        let rk = options_for(HardwareAccelerationType::rkmpp);
        let mut sized = stream("h264", "yuv420p10le");
        sized.width = Some(1920);
        sized.height = Some(1080);
        let mut with_size = ctx(&caps, &rk, &sized);
        with_size.requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };
        assert!(hardware_video_decoder(&with_size).is_some());

        // Without dimensions the ratio check cannot pass, so no hardware.
        assert_eq!(hardware_video_decoder(&ctx(&caps, &rk, &s)), None);
    }

    #[test]
    fn apple_silicon_gets_hi10p_from_macos_14_6() {
        let s = stream("h264", "yuv420p10le");
        let options = options_for(HardwareAccelerationType::videotoolbox);
        let build = |arm64: bool, os: FfmpegVersion| {
            FfmpegCapabilities::builder()
                .platform(Platform::MacOs)
                .hwaccels(["videotoolbox"])
                .filters(crate::encoder::REQUIRED_FILTERS)
                .all_filter_options(true)
                .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
                .arm64(arm64)
                .os_version(os)
                .build()
        };
        // Apple Silicon on 14.6: yes.
        let caps = build(true, FfmpegVersion::new(14, 6));
        assert!(hardware_video_decoder(&ctx(&caps, &options, &s)).is_some());
        // Apple Silicon on 14.5: no.
        let caps = build(true, FfmpegVersion::new(14, 5));
        assert_eq!(hardware_video_decoder(&ctx(&caps, &options, &s)), None);
        // Intel Mac on 15.0: no — the carve-out is arm64-only.
        let caps = build(false, FfmpegVersion::new(15, 0));
        assert_eq!(hardware_video_decoder(&ctx(&caps, &options, &s)), None);
    }

    #[test]
    fn h264_high_chroma_profiles_are_refused_off_apple_silicon() {
        let mut s = stream("h264", "yuv420p");
        s.profile = Some("High 4:4:4 Predictive".to_owned());
        let options = options_for(HardwareAccelerationType::nvenc);
        let caps = caps_for(Platform::Linux);
        assert_eq!(hardware_video_decoder(&ctx(&caps, &options, &s)), None);

        let mut s422 = stream("h264", "yuv420p");
        s422.profile = Some("High 4:2:2".to_owned());
        assert_eq!(hardware_video_decoder(&ctx(&caps, &options, &s422)), None);
    }

    #[test]
    fn vaapi_and_amf_are_hwaccel_only_with_no_named_decoder() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        let decoder = hardware_video_decoder(&ctx(&caps, &options, &s)).unwrap();
        assert!(!decoder.contains("-c:v"), "{decoder}");
        assert!(decoder.contains("-hwaccel vaapi"), "{decoder}");

        let caps = caps_for(Platform::Windows);
        let options = options_for(HardwareAccelerationType::amf);
        let decoder = hardware_video_decoder(&ctx(&caps, &options, &s)).unwrap();
        assert!(!decoder.contains("-c:v"), "{decoder}");
        assert!(decoder.contains("-hwaccel d3d11va"), "{decoder}");
    }

    #[test]
    fn rkmpp_adds_afbc_only_for_the_codecs_that_can_use_it() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::rkmpp);
        // A same-size job is within both the RGA2e and RGA3 ratio limits.
        let mut s = stream("h264", "yuv420p");
        s.width = Some(1920);
        s.height = Some(1080);
        let mut c = ctx(&caps, &options, &s);
        c.requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };
        let decoder = hardware_video_decoder(&c).unwrap();
        assert!(decoder.ends_with(" -afbc rga"), "{decoder}");

        // AV1 AFBC is broken on RK3588 and deliberately left off.
        let mut av1 = stream("av1", "yuv420p");
        av1.width = Some(1920);
        av1.height = Some(1080);
        let mut c = ctx(&caps, &options, &av1);
        c.requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };
        let decoder = hardware_video_decoder(&c).unwrap();
        assert!(!decoder.contains("-afbc"), "{decoder}");
    }

    #[test]
    fn rkmpp_refuses_a_scale_outside_the_rga_ratio() {
        let caps = caps_for(Platform::Linux);
        let options = options_for(HardwareAccelerationType::rkmpp);
        let mut s = stream("h264", "yuv420p");
        s.width = Some(3840);
        s.height = Some(2160);
        let mut c = ctx(&caps, &options, &s);
        // A 20x downscale is outside RGA2e's 1/16..16 window.
        c.requested = RequestedSize {
            width: Some(192),
            height: Some(108),
            ..RequestedSize::default()
        };
        assert_eq!(hardware_video_decoder(&c), None);
    }

    #[test]
    fn videotoolbox_av1_needs_the_platform_probe() {
        let s = stream("av1", "yuv420p10le");
        let options = options_for(HardwareAccelerationType::videotoolbox);
        let build = |av1: bool| {
            FfmpegCapabilities::builder()
                .platform(Platform::MacOs)
                .hwaccels(["videotoolbox"])
                .filters(crate::encoder::REQUIRED_FILTERS)
                .all_filter_options(true)
                .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
                .videotoolbox_av1_decode(av1)
                .build()
        };
        assert!(hardware_video_decoder(&ctx(&build(true), &options, &s)).is_some());
        assert_eq!(
            hardware_video_decoder(&ctx(&build(false), &options, &s)),
            None
        );
    }

    #[test]
    fn a_pixel_format_outside_the_vendors_set_gets_no_hardware() {
        let caps = caps_for(Platform::Linux);
        // NVDEC's wide set is 4:4:4 only — it has no 4:2:2 entry, unlike QSV.
        let options = options_for(HardwareAccelerationType::nvenc);
        let s = stream("hevc", "yuv422p10le");
        assert_eq!(hardware_video_decoder(&ctx(&caps, &options, &s)), None);
        // QSV accepts exactly that format.
        let mut qsv = options_for(HardwareAccelerationType::qsv);
        qsv.prefer_system_native_hw_decoder = false;
        assert!(hardware_video_decoder(&ctx(&caps, &qsv, &s)).is_some());
    }

    // ----- scaling helpers ---------------------------------------------------

    #[rstest]
    // A same-size request is untouched.
    #[case(Some(1920), Some(1080), None, None, (Some(1920), Some(1080)))]
    // Hardware never scales above 4K, even when asked: 8K bounded to a 4096
    // width keeps 16:9, so the height follows to 2304 rather than being
    // clamped independently.
    #[case(Some(7680), Some(4320), Some(7680), Some(4320), (Some(4096), Some(2304)))]
    // Odd dimensions round DOWN to even, which every hardware scaler needs.
    #[case(Some(1921), Some(1081), None, None, (Some(1920), Some(1080)))]
    // Without a width on either side there is no answer at all.
    #[case(None, Some(1080), None, None, (None, None))]
    #[case(Some(1920), None, None, None, (None, None))]
    fn fixed_output_size_caps_at_4k_and_rounds_to_even(
        #[case] in_w: Option<i32>,
        #[case] in_h: Option<i32>,
        #[case] max_w: Option<i32>,
        #[case] max_h: Option<i32>,
        #[case] expected: (Option<i32>, Option<i32>),
    ) {
        let requested = RequestedSize {
            max_width: max_w,
            max_height: max_h,
            ..RequestedSize::default()
        };
        assert_eq!(fixed_output_size(in_w, in_h, requested), expected);
    }

    #[test]
    fn a_max_bound_scales_both_dimensions_together() {
        // 1920x1080 bounded to 1280 wide keeps the aspect: 1280x720.
        let requested = RequestedSize {
            max_width: Some(1280),
            max_height: Some(720),
            ..RequestedSize::default()
        };
        assert_eq!(
            fixed_output_size(Some(1920), Some(1080), requested),
            (Some(1280), Some(720))
        );
    }

    #[rstest]
    // Same size: ratio 1, inside every window.
    #[case(1920, 1080, Some(1920), Some(1080), 16.0, true)]
    // 2x down: inside 1/16..16.
    #[case(1920, 1080, Some(960), Some(540), 16.0, true)]
    // 20x down: outside RGA2e's window.
    #[case(3840, 2160, Some(192), Some(108), 16.0, false)]
    // 10x down: inside RGA2e (16) but outside RGA3/AFBC (8).
    #[case(3840, 2160, Some(384), Some(216), 16.0, true)]
    #[case(3840, 2160, Some(384), Some(216), 8.0, false)]
    // A ratio limit below 1 is nonsense and is refused.
    #[case(1920, 1080, Some(1920), Some(1080), 0.5, false)]
    fn scale_ratio_windows_match_the_rga_blocks(
        #[case] in_w: i32,
        #[case] in_h: i32,
        #[case] req_w: Option<i32>,
        #[case] req_h: Option<i32>,
        #[case] max_ratio: f64,
        #[case] expected: bool,
    ) {
        let requested = RequestedSize {
            width: req_w,
            height: req_h,
            ..RequestedSize::default()
        };
        assert_eq!(
            is_scale_ratio_supported(Some(in_w), Some(in_h), requested, max_ratio),
            expected
        );
    }

    #[test]
    fn an_unknown_source_size_fails_the_scale_check() {
        let requested = RequestedSize {
            width: Some(1280),
            height: Some(720),
            ..RequestedSize::default()
        };
        assert!(!is_scale_ratio_supported(None, Some(1080), requested, 16.0));
        assert!(!is_scale_ratio_supported(Some(1920), None, requested, 16.0));
    }

    #[rstest]
    // `Convert.ToInt32` rounds half to EVEN, not half away from zero.
    #[case(0.5, 0)]
    #[case(1.5, 2)]
    #[case(2.5, 2)]
    #[case(3.5, 4)]
    // Anything off the midpoint rounds normally.
    #[case(1279.4, 1279)]
    #[case(1279.6, 1280)]
    #[case(-1.5, -2)]
    #[case(-2.5, -2)]
    fn rounding_is_bankers_rounding_like_dotnet(#[case] value: f64, #[case] expected: i32) {
        assert_eq!(round_to_i32(value), expected);
    }

    #[test]
    fn videotoolbox_ignores_the_colour_depth_switches() {
        // Its decoders fall back to software internally rather than failing, so
        // upstream exempts the whole depth block for VideoToolbox. With the
        // switch off, every other backend refuses; VideoToolbox does not.
        let mut options = options_for(HardwareAccelerationType::videotoolbox);
        options.enable_decoding_color_depth10_hevc = false;
        let s = stream("hevc", "yuv420p10le");

        let mac = caps_for(Platform::MacOs);
        assert!(
            hwaccel_type(&ctx(&mac, &options, &s), "hevc", 10, false).is_some(),
            "VideoToolbox must ignore the 10-bit HEVC switch"
        );

        // The same switch on VAAPI does refuse.
        let mut vaapi = options.clone();
        vaapi.hardware_acceleration_type = HardwareAccelerationType::vaapi;
        let linux = caps_for(Platform::Linux);
        assert_eq!(
            hwaccel_type(&ctx(&linux, &vaapi, &s), "hevc", 10, false),
            None
        );
    }

    #[test]
    fn nvdec_has_no_four_two_two_tier_where_qsv_does() {
        // NVDEC's wide format set is 4:4:4 only. A 4:2:2 source is also HEVC
        // RExt, so the RExt switch has to be ON for the tier itself to be what
        // decides — otherwise both vendors refuse for the other reason.
        let caps = caps_for(Platform::Linux);
        let s = stream("hevc", "yuv422p10le");

        let mut nvenc = options_for(HardwareAccelerationType::nvenc);
        nvenc.enable_decoding_color_depth10_hevc_rext = true;
        assert_eq!(
            hardware_video_decoder(&ctx(&caps, &nvenc, &s)),
            None,
            "NVDEC has no 4:2:2 tier"
        );

        let mut qsv = options_for(HardwareAccelerationType::qsv);
        qsv.enable_decoding_color_depth10_hevc_rext = true;
        qsv.prefer_system_native_hw_decoder = false;
        // QSV's tier does include 4:2:2. (The iHD-driver rule in the RExt gate
        // only fires when the accelerator IS vaapi, so it does not apply here.)
        assert!(
            hardware_video_decoder(&ctx(&caps, &qsv, &s)).is_some(),
            "QSV's tier includes 4:2:2"
        );
    }

    #[test]
    fn ten_bit_rkmpp_needs_a_hardware_surface() {
        // nv15/nv20 are bitstream-only formats, so a 10-bit RKMPP decode
        // without a hardware surface has nowhere to put the frames.
        let options = options_for(HardwareAccelerationType::rkmpp);
        let mut s = stream("hevc", "yuv420p10le");
        s.width = Some(1920);
        s.height = Some(1080);
        let requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };

        // Full RKMPP + OpenCL + alphasrc: a surface is available.
        let full = caps_for(Platform::Linux);
        let mut c = ctx(&full, &options, &s);
        c.requested = requested;
        assert!(hardware_video_decoder(&c).is_some());

        // The same machine without `alphasrc` has no surface, so 10-bit is out.
        let no_alphasrc = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["rkmpp", "opencl"])
            .filters(
                crate::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "alphasrc"),
            )
            .decoders(REQUIRED_DECODERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let mut c = ctx(&no_alphasrc, &options, &s);
        c.requested = requested;
        assert_eq!(hardware_video_decoder(&c), None);

        // An 8-bit source on that same machine is fine — the restriction is
        // specific to the 10-bit bitstream formats.
        let mut eight = stream("hevc", "yuv420p");
        eight.width = Some(1920);
        eight.height = Some(1080);
        let mut c = ctx(&no_alphasrc, &options, &eight);
        c.requested = requested;
        assert!(hardware_video_decoder(&c).is_some());
    }

    #[test]
    fn a_capability_gap_disables_the_backend_entirely() {
        // CUDA without `hwupload_cuda` is not "full support", so NVENC offers
        // no hardware decode at all — the check the whole matrix rests on.
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda"])
            .filters(
                crate::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "hwupload_cuda"),
            )
            .decoders(REQUIRED_DECODERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options_for(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, true),
            None
        );
    }

    #[test]
    fn the_filter_option_probes_reach_the_decoder_decision() {
        // `ScaleCudaFormat` is one of the conjuncts of CUDA full support, so a
        // build whose scale_cuda lacks `format` gets no CUDA decode.
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda"])
            .filters(crate::encoder::REQUIRED_FILTERS)
            .decoders(REQUIRED_DECODERS)
            .all_filter_options(true)
            .filter_option(FilterOption::ScaleCudaFormat, false)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options_for(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        assert_eq!(
            hwaccel_type(&ctx(&caps, &options, &s), "h264", 8, true),
            None
        );
    }
}
