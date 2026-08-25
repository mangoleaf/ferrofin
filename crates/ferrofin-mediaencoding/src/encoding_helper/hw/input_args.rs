//! Everything ffmpeg needs before `-i` — the hardware half of the input line.
//!
//! Port of C# `EncodingHelper.GetInputVideoHwaccelArgs` (10.11.z lines
//! 995–1228) and `GetGraphicalSubCanvasSize` (962–994), plus the
//! `IsHwTonemapAvailable` predicate the dispatcher consults (349–383).
//!
//! This is where the pieces meet. For the operator's chosen accelerator it
//! decides which devices to initialise and in what order, which of them the
//! *filter* graph should run on, what environment the ffmpeg process needs, and
//! finally appends the decoder arguments. Every branch first checks that the
//! job will actually touch that hardware — a VAAPI device is pointless if
//! neither the decoder nor the encoder is VAAPI — because initialising a device
//! that is never used still fails the whole command on a machine without it.
//!
//! **Environment variables are returned, not set.** C# calls
//! `Environment.SetEnvironmentVariable`, which mutates the whole server
//! process; here they ride along in [`InputHwaccelArgs::env`] for the caller to
//! put on the ffmpeg child alone. Same effect on ffmpeg, no global state.

use ferrofin_model::data::{VideoRange, VideoRangeType};
use ferrofin_model::entities::HardwareAccelerationType;
use ferrofin_model::entities_media::MediaStream;

use super::decoder::{DecodeContext, hardware_video_decoder, video_color_bit_depth};
use super::device_init::{
    CUDA_ALIAS, D3D11VA_ALIAS, DRM_ALIAS, OPENCL_ALIAS, OPENCL_VENDOR_AMD, QSV_ALIAS, RKMPP_ALIAS,
    RenderNode, VAAPI_ALIAS, VENDOR_ID_AMD, VIDEOTOOLBOX_ALIAS, VULKAN_ALIAS, VaapiDeviceSpec,
    cuda_device_args, d3d11va_device_args, drm_device_args, filter_hw_device_args,
    opencl_device_args, qsv_device_args, rkmpp_device_args, vaapi_device_args,
    videotoolbox_device_args, vulkan_device_args,
};
use super::support::{is_cuda_full_supported, is_opencl_full_supported, is_vulkan_full_supported};
use super::versions::{MIN_FFMPEG_RKMPP_HEVC_DEC_DOVI_RPU, MIN_KERNEL_VERSION_AMD_VK_FMT_MODIFIER};

/// The subtitle canvas ffmpeg should render graphical subtitles onto.
///
/// Port of the `DVBSUB` special case in `GetGraphicalSubCanvasSize`: DVB
/// subtitles carry no dimensions of their own and are always 720x576, so
/// upstream declines to emit a canvas for them rather than emitting a wrong one.
pub const DVBSUB_CODEC: &str = "DVBSUB";

/// The hardware input arguments, with the environment the ffmpeg child needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputHwaccelArgs {
    /// The argument string, already trimmed. Empty when no hardware applies.
    pub args: String,
    /// Environment variables to set **on the ffmpeg process**, not on the
    /// server. Port of the `Environment.SetEnvironmentVariable` calls in the
    /// VAAPI branch.
    pub env: Vec<(String, String)>,
}

/// Whether hardware tonemapping can run for this job. Port of
/// `IsHwTonemapAvailable`.
///
/// `video_decoder` is the already-computed decoder argument; C# recomputes it
/// inside this predicate, which is the same value for the same inputs. The
/// `EnableTonemapping` switch is read from `ctx.options` rather than passed, so
/// a caller cannot hand in a value that contradicts the options it also passed.
///
/// Dolby Vision is the interesting case: its dynamic metadata lives in an RPU
/// that only some decoders parse. A source that is DOVI *and* decoded by
/// something that cannot read the RPU has nothing for the tonemapper to work
/// from, so hardware tonemapping is refused and the software path handles it.
#[must_use]
pub fn is_hw_tonemap_available(ctx: &DecodeContext<'_>, video_decoder: &str) -> bool {
    let Some(stream) = ctx.video_stream else {
        return false;
    };
    if !ctx.options.enable_tonemapping || video_color_bit_depth(Some(stream)) < 10 {
        return false;
    }
    if stream.video_range != Some(VideoRange::Hdr) {
        return false;
    }

    if stream.video_range_type == Some(VideoRangeType::Dovi) {
        // hevc_rkmpp learned to parse Dolby Vision RPUs in ffmpeg 7.1.1.
        let is_rkmpp = contains(video_decoder, "rkmpp");
        if is_rkmpp
            && ctx.caps.ffmpeg_at_least(MIN_FFMPEG_RKMPP_HEVC_DEC_DOVI_RPU)
            && stream.codec.as_deref().is_some_and(|c| eq(c, "hevc"))
        {
            return true;
        }
        // Otherwise only the native software decoder and these accelerators
        // surface the RPU.
        let is_sw = video_decoder.is_empty();
        return is_sw
            || contains(video_decoder, "cuda")
            || contains(video_decoder, "vaapi")
            || contains(video_decoder, "d3d11va")
            || contains(video_decoder, "videotoolbox");
    }

    // Every other HDR flavour tonemaps on the GPU.
    true
}

/// The ` -canvas_size WxH` argument for graphical subtitle overlay, or an empty
/// string. Port of `GetGraphicalSubCanvasSize`.
///
/// Only emitted for a *bitmap* subtitle that will be burned in and carries real
/// dimensions; text subtitles are rendered by the `subtitles` filter, which
/// needs no canvas, and DVBSUB's fixed 720x576 is left to ffmpeg.
#[must_use]
pub fn graphical_sub_canvas_size(
    subtitle_stream: Option<&MediaStream>,
    should_encode_subtitle: bool,
) -> String {
    let Some(sub) = subtitle_stream else {
        return String::new();
    };
    if !should_encode_subtitle
        || sub.is_text_subtitle_stream.unwrap_or(false)
        || sub.codec.as_deref().is_some_and(|c| eq(c, DVBSUB_CODEC))
    {
        return String::new();
    }
    match (sub.width, sub.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => format!(" -canvas_size {w}x{h}"),
        _ => String::new(),
    }
}

/// The `-init_hw_device` graph, `-filter_hw_device` selection and decoder
/// arguments for this job. Port of `GetInputVideoHwaccelArgs`.
///
/// `video_encoder` is the already-selected encoder name (see
/// [`super::encoder::video_encoder`]). The two device settings are separate and
/// are each resolved by the caller: `render_node` is `EncodingOptions.
/// VaapiDevice`, and `qsv_node` is `EncodingOptions.QsvDevice` — which on Linux
/// is a render-node path in its own right (and on Windows an adapter index,
/// where the usability flag is simply unread).
///
/// `is_video_request` is C#'s `state.IsVideoRequest`, which in this codebase is
/// [`EncodingJobInfo::is_input_video`](crate::EncodingJobInfo): an audio-only
/// job has no video to decode, and initialising a device for it would fail the
/// whole ffmpeg command on a machine without that hardware.
#[must_use]
pub fn input_video_hwaccel_args(
    ctx: &DecodeContext<'_>,
    video_encoder: &str,
    render_node: RenderNode<'_>,
    qsv_node: RenderNode<'_>,
    is_video_request: bool,
) -> InputHwaccelArgs {
    if !is_video_request || eq(video_encoder, "copy") {
        return InputHwaccelArgs::default();
    }

    let video_decoder = hardware_video_decoder(ctx).unwrap_or_default();
    let job = HwInputJob {
        ctx,
        video_encoder,
        video_decoder: &video_decoder,
        render_node,
        qsv_node,
        hw_tonemap: is_hw_tonemap_available(ctx, &video_decoder),
    };

    // `None` here means the branch **bailed**, which in C# is a bare
    // `return string.Empty` — it exits the whole method, so the decoder is NOT
    // appended. V4L2M2M and "none" have no branch at all and fall through to
    // the tail instead (their decoder is always empty, so the append is a
    // no-op, but keeping the shape keeps the two cases distinguishable).
    let branch = match ctx.options.hardware_acceleration_type {
        HardwareAccelerationType::vaapi => vaapi_input_args(&job),
        HardwareAccelerationType::qsv => qsv_input_args(&job),
        HardwareAccelerationType::nvenc => nvenc_input_args(&job),
        HardwareAccelerationType::amf => amf_input_args(&job),
        HardwareAccelerationType::videotoolbox => videotoolbox_input_args(&job),
        HardwareAccelerationType::rkmpp => rkmpp_input_args(&job),
        HardwareAccelerationType::v4l2m2m | HardwareAccelerationType::none => {
            Some(InputHwaccelArgs::default())
        }
    };
    let Some(mut out) = branch else {
        return InputHwaccelArgs::default();
    };

    if !video_decoder.is_empty() {
        out.args.push_str(&video_decoder);
    }
    out.args = out.args.trim().to_owned();
    out
}

/// The inputs every per-vendor branch reads.
#[derive(Debug, Clone, Copy)]
struct HwInputJob<'a> {
    ctx: &'a DecodeContext<'a>,
    video_encoder: &'a str,
    video_decoder: &'a str,
    render_node: RenderNode<'a>,
    qsv_node: RenderNode<'a>,
    hw_tonemap: bool,
}

impl HwInputJob<'_> {
    /// Whether the decoder argument names this backend.
    fn decoder_is(&self, needle: &str) -> bool {
        contains(self.video_decoder, needle)
    }

    /// Whether the selected encoder names this backend.
    fn encoder_is(&self, needle: &str) -> bool {
        contains(self.video_encoder, needle)
    }

    /// Whether OpenCL tonemapping applies — the condition that pulls an OpenCL
    /// device into three of the six graphs.
    fn ocl_tonemap(&self) -> bool {
        self.hw_tonemap && is_opencl_full_supported(self.ctx.caps)
    }
}

/// The VAAPI device graph. Port of the `vaapi` branch of
/// `GetInputVideoHwaccelArgs`.
fn vaapi_input_args(job: &HwInputJob<'_>) -> Option<InputHwaccelArgs> {
    let caps = job.ctx.caps;
    if !caps.platform().is_linux() || !caps.supports_hwaccel("vaapi") {
        return None;
    }
    let is_vaapi_decoder = job.decoder_is("vaapi");
    if !is_vaapi_decoder && !job.encoder_is("vaapi") {
        return None;
    }

    let version = caps.ffmpeg_version();
    let mut out = InputHwaccelArgs::default();

    if caps.is_vaapi_device_intel_ihd() {
        out.args.push_str(&vaapi_device_args(
            &VaapiDeviceSpec::for_render_node(job.render_node).with_driver(Some("iHD")),
            VAAPI_ALIAS,
            version,
        ));
    } else if caps.is_vaapi_device_intel_i965() {
        // i965 ranks below iHD in libva's own lookup, so it has to be named
        // explicitly or libva picks the other one.
        out.env
            .push(("LIBVA_DRIVER_NAME".to_owned(), "i965".to_owned()));
        out.env
            .push(("LIBVA_DRIVER_NAME_JELLYFIN".to_owned(), "i965".to_owned()));
        out.args.push_str(&vaapi_device_args(
            &VaapiDeviceSpec::for_render_node(job.render_node).with_driver(Some("i965")),
            VAAPI_ALIAS,
            version,
        ));
    }

    let mut filter_dev = String::new();
    let do_ocl_tonemap = job.ocl_tonemap();

    if caps.is_vaapi_device_intel_ihd() || caps.is_vaapi_device_intel_i965() {
        if do_ocl_tonemap && !is_vaapi_decoder {
            out.args.push_str(&opencl_device_args(
                0,
                None,
                Some(VAAPI_ALIAS),
                OPENCL_ALIAS,
            ));
            filter_dev = filter_hw_device_args(Some(OPENCL_ALIAS));
        }
    } else if caps.is_vaapi_device_amd() {
        // AMD's Efficient Frame Compression is still unstable in Mesa.
        out.env.push(("AMD_DEBUG".to_owned(), "noefc".to_owned()));

        if is_vulkan_full_supported(caps)
            && caps.vaapi_vulkan_drm_interop()
            && caps
                .os_version()
                .is_some_and(|v| v >= MIN_KERNEL_VERSION_AMD_VK_FMT_MODIFIER)
        {
            out.args
                .push_str(&drm_device_args(job.render_node.path(), DRM_ALIAS));
            out.args.push_str(&vaapi_device_args(
                &VaapiDeviceSpec {
                    src_device_alias: Some(DRM_ALIAS),
                    ..VaapiDeviceSpec::default()
                },
                VAAPI_ALIAS,
                version,
            ));
            out.args
                .push_str(&vulkan_device_args(0, None, Some(DRM_ALIAS), VULKAN_ALIAS));
            // libplacebo insists on an explicit vulkan filter device.
            filter_dev = filter_hw_device_args(Some(VULKAN_ALIAS));
        } else {
            out.args.push_str(&vaapi_device_args(
                &VaapiDeviceSpec::for_render_node(job.render_node),
                VAAPI_ALIAS,
                version,
            ));
            filter_dev = filter_hw_device_args(Some(VAAPI_ALIAS));

            if do_ocl_tonemap {
                // The ROCm/ROCr OpenCL runtime.
                out.args.push_str(&opencl_device_args(
                    0,
                    Some(OPENCL_VENDOR_AMD),
                    None,
                    OPENCL_ALIAS,
                ));
                filter_dev = filter_hw_device_args(Some(OPENCL_ALIAS));
            }
        }
    } else if do_ocl_tonemap {
        out.args
            .push_str(&opencl_device_args(0, None, None, OPENCL_ALIAS));
        filter_dev = filter_hw_device_args(Some(OPENCL_ALIAS));
    }

    out.args.push_str(&filter_dev);
    Some(out)
}

/// The Quick Sync device graph. Port of the `qsv` branch.
fn qsv_input_args(job: &HwInputJob<'_>) -> Option<InputHwaccelArgs> {
    let caps = job.ctx.caps;
    let platform = caps.platform();
    if (!platform.is_linux() && !platform.is_windows()) || !caps.supports_hwaccel("qsv") {
        return None;
    }
    // QSV accepts frames from any of the three Intel decode paths.
    let is_hw_decoder =
        job.decoder_is("qsv") || job.decoder_is("vaapi") || job.decoder_is("d3d11va");
    if !is_hw_decoder && !job.encoder_is("qsv") {
        return None;
    }

    let mut out = InputHwaccelArgs::default();
    if let Some(device) = qsv_device_args(job.qsv_node, QSV_ALIAS, platform, caps.ffmpeg_version())
    {
        out.args.push_str(&device);
    }
    let mut filter_dev = filter_hw_device_args(Some(QSV_ALIAS));

    // The child OpenCL device, derived from whichever device QSV came from.
    if (caps.supports_hwaccel("vaapi") || caps.supports_hwaccel("d3d11va")) && job.ocl_tonemap() {
        let src = if platform.is_linux() {
            VAAPI_ALIAS
        } else {
            D3D11VA_ALIAS
        };
        out.args
            .push_str(&opencl_device_args(0, None, Some(src), OPENCL_ALIAS));
        if !is_hw_decoder {
            filter_dev = filter_hw_device_args(Some(OPENCL_ALIAS));
        }
    }

    out.args.push_str(&filter_dev);
    Some(out)
}

/// The CUDA device graph. Port of the `nvenc` branch — the simplest of the six.
fn nvenc_input_args(job: &HwInputJob<'_>) -> Option<InputHwaccelArgs> {
    let caps = job.ctx.caps;
    let platform = caps.platform();
    if (!platform.is_linux() && !platform.is_windows()) || !is_cuda_full_supported(caps) {
        return None;
    }
    if !job.decoder_is("cuda") && !job.decoder_is("cuvid") && !job.encoder_is("nvenc") {
        return None;
    }
    Some(InputHwaccelArgs {
        args: format!(
            "{}{}",
            cuda_device_args(0, CUDA_ALIAS),
            filter_hw_device_args(Some(CUDA_ALIAS))
        ),
        env: Vec::new(),
    })
}

/// The AMF device graph. Port of the `amf` branch.
fn amf_input_args(job: &HwInputJob<'_>) -> Option<InputHwaccelArgs> {
    let caps = job.ctx.caps;
    if !caps.platform().is_windows() || !caps.supports_hwaccel("d3d11va") {
        return None;
    }
    if !job.decoder_is("d3d11va") && !job.encoder_is("amf") {
        return None;
    }
    // There is no DXVA video-processor filter, so OpenCL does the filtering.
    let mut out = InputHwaccelArgs {
        args: d3d11va_device_args(0, Some(VENDOR_ID_AMD), D3D11VA_ALIAS),
        env: Vec::new(),
    };
    if is_opencl_full_supported(caps) {
        out.args.push_str(&opencl_device_args(
            0,
            None,
            Some(D3D11VA_ALIAS),
            OPENCL_ALIAS,
        ));
        out.args
            .push_str(&filter_hw_device_args(Some(OPENCL_ALIAS)));
    }
    Some(out)
}

/// The VideoToolbox device. Port of the `videotoolbox` branch.
fn videotoolbox_input_args(job: &HwInputJob<'_>) -> Option<InputHwaccelArgs> {
    let caps = job.ctx.caps;
    if !caps.platform().is_macos() || !caps.supports_hwaccel("videotoolbox") {
        return None;
    }
    if !job.decoder_is("videotoolbox") && !job.encoder_is("videotoolbox") {
        return None;
    }
    // VideoToolbox filters need no device selection.
    Some(InputHwaccelArgs {
        args: videotoolbox_device_args(VIDEOTOOLBOX_ALIAS),
        env: Vec::new(),
    })
}

/// The Rockchip device graph. Port of the `rkmpp` branch.
fn rkmpp_input_args(job: &HwInputJob<'_>) -> Option<InputHwaccelArgs> {
    let caps = job.ctx.caps;
    if !caps.platform().is_linux() || !caps.supports_hwaccel("rkmpp") {
        return None;
    }
    let is_rkmpp_decoder = job.decoder_is("rkmpp");
    if !is_rkmpp_decoder && !job.encoder_is("rkmpp") {
        return None;
    }

    let mut out = InputHwaccelArgs {
        args: rkmpp_device_args(RKMPP_ALIAS),
        env: Vec::new(),
    };
    if job.ocl_tonemap() && !is_rkmpp_decoder {
        out.args.push_str(&opencl_device_args(
            0,
            None,
            Some(RKMPP_ALIAS),
            OPENCL_ALIAS,
        ));
        out.args
            .push_str(&filter_hw_device_args(Some(OPENCL_ALIAS)));
    }
    Some(out)
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// `string.Contains(x, StringComparison.OrdinalIgnoreCase)`.
///
/// Both sides are lowered, so this really is case-insensitive rather than
/// relying on every call site passing a lowercase literal.
fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{FfmpegVersion, REQUIRED_DECODERS, REQUIRED_FILTERS};
    use crate::encoding_helper::hw::capabilities::{FfmpegCapabilities, Platform};
    use crate::encoding_helper::hw::decoder::RequestedSize;
    use ferrofin_model::configuration::EncodingOptions;
    use ferrofin_model::entities::HardwareAccelerationType;

    // Hand-derived from the C# `GetInputVideoHwaccelArgs` (10.11.z 995-1228);
    // upstream ships no tests for it. These are the whole-line goldens: what
    // ffmpeg actually receives before `-i`.

    const V701: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

    /// A build with every hwaccel, filter and decoder, on `platform`.
    fn caps(platform: Platform) -> FfmpegCapabilities {
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
            .filters(REQUIRED_FILTERS)
            .decoders(REQUIRED_DECODERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .build()
    }

    fn options(hw_type: HardwareAccelerationType) -> EncodingOptions {
        let mut options = EncodingOptions {
            hardware_acceleration_type: hw_type,
            ..EncodingOptions::default()
        };
        options.hardware_decoding_codecs = ["h264", "hevc", "vp9", "av1", "vc1"]
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

    /// The dispatcher for a video request with no QSV device configured — the
    /// shape almost every test wants.
    fn args_for(
        ctx: &DecodeContext<'_>,
        video_encoder: &str,
        render_node: RenderNode<'_>,
    ) -> InputHwaccelArgs {
        input_video_hwaccel_args(ctx, video_encoder, render_node, RenderNode::default(), true)
    }

    /// A render node that exists, so VAAPI selects it by path.
    fn node() -> RenderNode<'static> {
        RenderNode::new(Some("/dev/dri/renderD128"), true)
    }

    #[test]
    fn nvenc_initialises_cuda_and_points_the_filters_at_it() {
        let caps = caps(Platform::Linux);
        let options = options(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_nvenc", node());
        assert_eq!(
            out.args,
            "-init_hw_device cuda=cu:0 -filter_hw_device cu -hwaccel cuda \
             -hwaccel_output_format cuda -noautorotate -hwaccel_flags +unsafe_output -threads 1"
        );
        assert!(out.env.is_empty());
    }

    #[test]
    fn vaapi_on_an_intel_ihd_device_names_the_driver() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(false, true, false)
            .build();
        let options = options(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_vaapi", node());
        assert_eq!(
            out.args,
            "-init_hw_device vaapi=va:/dev/dri/renderD128,driver=iHD \
             -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate"
        );
        assert!(out.env.is_empty(), "iHD needs no driver override");
    }

    #[test]
    fn vaapi_on_a_legacy_i965_device_overrides_the_libva_driver() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(false, false, true)
            .build();
        let options = options(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_vaapi", node());
        assert!(
            out.args
                .starts_with("-init_hw_device vaapi=va:/dev/dri/renderD128,driver=i965"),
            "{}",
            out.args
        );
        // i965 sits below iHD in libva's own lookup, so it has to be named on
        // the child process rather than left to libva.
        assert_eq!(
            out.env,
            vec![
                ("LIBVA_DRIVER_NAME".to_owned(), "i965".to_owned()),
                ("LIBVA_DRIVER_NAME_JELLYFIN".to_owned(), "i965".to_owned()),
            ]
        );
    }

    #[test]
    fn vaapi_on_amd_disables_efc_and_can_chain_drm_vaapi_vulkan() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "vulkan", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(true, false, false)
            .vaapi_vulkan(true, true)
            .os_version(FfmpegVersion::with_revision(6, 1, 0, 0))
            .build();
        let options = options(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_vaapi", node());
        // DRM first, then VAAPI and Vulkan derived from it so all three share
        // buffers; libplacebo needs the filter device named explicitly.
        assert_eq!(
            out.args,
            "-init_hw_device drm=dr:/dev/dri/renderD128 -init_hw_device vaapi=va@dr \
             -init_hw_device vulkan=vk@dr -filter_hw_device vk \
             -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate"
        );
        assert_eq!(
            out.env,
            vec![("AMD_DEBUG".to_owned(), "noefc".to_owned())],
            "AMD's Efficient Frame Compression is unstable in Mesa"
        );
    }

    #[test]
    fn an_amd_kernel_below_5_15_takes_the_plain_vaapi_path() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "vulkan", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(true, false, false)
            .vaapi_vulkan(true, true)
            .os_version(FfmpegVersion::with_revision(5, 14, 0, 0))
            .build();
        let options = options(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_vaapi", node());
        assert_eq!(
            out.args,
            "-init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va \
             -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate"
        );
    }

    #[test]
    fn qsv_derives_its_device_from_the_platform_and_names_the_filter_device() {
        let options = options(HardwareAccelerationType::qsv);
        let s = stream("h264", "yuv420p");

        let linux = caps(Platform::Linux);
        let out = args_for(&ctx(&linux, &options, &s), "h264_qsv", node());
        assert_eq!(
            out.args,
            // `QsvDevice` defaults to empty, so the VAAPI device it derives
            // from is pinned by Intel's vendor id rather than by a path.
            "-init_hw_device vaapi=va:,vendor_id=0x8086,driver=iHD -init_hw_device qsv=qs@va \
             -filter_hw_device qs -hwaccel vaapi -hwaccel_output_format vaapi -noautorotate"
        );

        let windows = caps(Platform::Windows);
        let out = args_for(&ctx(&windows, &options, &s), "h264_qsv", node());
        assert_eq!(
            out.args,
            "-init_hw_device d3d11va=dx11:,vendor=0x8086 -init_hw_device qsv=qs@dx11 \
             -filter_hw_device qs -hwaccel d3d11va -hwaccel_output_format d3d11 \
             -noautorotate -threads 2"
        );
    }

    #[test]
    fn amf_pins_the_amd_adapter_and_filters_through_opencl() {
        let caps = caps(Platform::Windows);
        let options = options(HardwareAccelerationType::amf);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_amf", node());
        assert_eq!(
            out.args,
            "-init_hw_device d3d11va=dx11:,vendor=0x1002 -init_hw_device opencl=ocl@dx11 \
             -filter_hw_device ocl -hwaccel d3d11va -hwaccel_output_format d3d11 \
             -noautorotate -threads 2"
        );
    }

    #[test]
    fn videotoolbox_needs_no_device_selection() {
        let caps = caps(Platform::MacOs);
        let options = options(HardwareAccelerationType::videotoolbox);
        let s = stream("hevc", "yuv420p10le");
        let out = args_for(&ctx(&caps, &options, &s), "hevc_videotoolbox", node());
        assert_eq!(
            out.args,
            "-init_hw_device videotoolbox=vt -hwaccel videotoolbox \
             -hwaccel_output_format videotoolbox_vld -noautorotate"
        );
    }

    #[test]
    fn rkmpp_initialises_its_device_and_decodes_to_drm_prime() {
        let caps = caps(Platform::Linux);
        let options = options(HardwareAccelerationType::rkmpp);
        let mut s = stream("h264", "yuv420p");
        s.width = Some(1920);
        s.height = Some(1080);
        let mut c = ctx(&caps, &options, &s);
        c.requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };
        let out = args_for(&c, "h264_rkmpp", node());
        assert_eq!(
            out.args,
            "-init_hw_device rkmpp=rk -hwaccel rkmpp -hwaccel_output_format drm_prime \
             -noautorotate -afbc rga"
        );
    }

    #[test]
    fn a_stream_copy_touches_no_hardware() {
        let caps = caps(Platform::Linux);
        let options = options(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "copy", node());
        assert_eq!(out.args, "");
        assert!(out.env.is_empty());
    }

    #[test]
    fn a_job_that_touches_neither_the_decoder_nor_the_encoder_gets_nothing() {
        // NVENC selected, but this job decodes and encodes in software: the
        // CUDA device would be initialised for nothing, and on a machine
        // without the GPU that alone fails the whole command.
        let caps = caps(Platform::Linux);
        let mut options = options(HardwareAccelerationType::nvenc);
        // Take the decoder away by disabling hardware decoding for the codec.
        options.hardware_decoding_codecs = vec!["vc1".to_owned()];
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "libx264", node());
        assert_eq!(out.args, "");
    }

    #[test]
    fn a_branch_that_bails_emits_nothing_at_all_not_even_the_decoder() {
        // The distinction C#'s `return string.Empty` makes: bailing exits the
        // whole method, so the decoder argument is dropped too.
        let caps = caps(Platform::Windows); // VAAPI is Linux-only
        let options = options(HardwareAccelerationType::vaapi);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_vaapi", node());
        assert_eq!(out.args, "");
    }

    #[test]
    fn tonemapping_pulls_an_opencl_device_into_the_graph() {
        // A software-decoded HDR source on an Intel iHD device: the OpenCL
        // device is derived from VAAPI and becomes the filter device.
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(false, true, false)
            .build();
        let mut options = options(HardwareAccelerationType::vaapi);
        options.enable_tonemapping = true;
        // No hardware decode for this codec, so the decoder is software.
        options.hardware_decoding_codecs = vec!["vc1".to_owned()];

        let mut s = stream("hevc", "yuv420p10le");
        s.video_range = Some(VideoRange::Hdr);
        s.video_range_type = Some(VideoRangeType::Hdr10);

        let out = args_for(&ctx(&caps, &options, &s), "hevc_vaapi", node());
        assert_eq!(
            out.args,
            "-init_hw_device vaapi=va:/dev/dri/renderD128,driver=iHD \
             -init_hw_device opencl=ocl@va -filter_hw_device ocl"
        );
    }

    #[test]
    fn amd_tonemapping_without_vulkan_uses_the_rocm_opencl_runtime() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(true, false, false)
            .build();
        let mut options = options(HardwareAccelerationType::vaapi);
        options.enable_tonemapping = true;
        options.hardware_decoding_codecs = vec!["vc1".to_owned()];

        let mut s = stream("hevc", "yuv420p10le");
        s.video_range = Some(VideoRange::Hdr);
        s.video_range_type = Some(VideoRangeType::Hdr10);

        let out = args_for(&ctx(&caps, &options, &s), "hevc_vaapi", node());
        assert_eq!(
            out.args,
            "-init_hw_device vaapi=va:/dev/dri/renderD128 \
             -init_hw_device opencl=ocl:.0,device_vendor=\"Advanced Micro Devices\" \
             -filter_hw_device ocl"
        );
    }

    // ----- the tonemap availability predicate --------------------------------

    /// HDR10 options with tonemapping switched on.
    fn tonemap_options(hw_type: HardwareAccelerationType) -> EncodingOptions {
        let mut options = options(hw_type);
        options.enable_tonemapping = true;
        options
    }

    /// A 10-bit HDR10 source.
    fn hdr_stream() -> MediaStream {
        let mut s = stream("hevc", "yuv420p10le");
        s.video_range = Some(VideoRange::Hdr);
        s.video_range_type = Some(VideoRangeType::Hdr10);
        s
    }

    #[test]
    fn hardware_tonemapping_needs_hdr_ten_bits_and_the_setting() {
        let caps = caps(Platform::Linux);
        let on = tonemap_options(HardwareAccelerationType::vaapi);
        let s = hdr_stream();
        assert!(is_hw_tonemap_available(&ctx(&caps, &on, &s), ""));

        // The setting is off by default.
        let off = options(HardwareAccelerationType::vaapi);
        assert!(!is_hw_tonemap_available(&ctx(&caps, &off, &s), ""));

        // An SDR source has nothing to tonemap.
        let mut sdr = s.clone();
        sdr.video_range = Some(VideoRange::Sdr);
        assert!(!is_hw_tonemap_available(&ctx(&caps, &on, &sdr), ""));

        // An 8-bit source is not HDR in practice.
        let mut eight = s.clone();
        eight.pixel_format = Some("yuv420p".to_owned());
        assert!(!is_hw_tonemap_available(&ctx(&caps, &on, &eight), ""));
    }

    #[test]
    fn dolby_vision_needs_a_decoder_that_can_read_the_rpu() {
        let caps = caps(Platform::Linux);
        let options = tonemap_options(HardwareAccelerationType::vaapi);
        let mut s = hdr_stream();
        s.video_range_type = Some(VideoRangeType::Dovi);
        let c = ctx(&caps, &options, &s);

        // The software decoder and these four accelerators surface the RPU.
        assert!(is_hw_tonemap_available(&c, ""), "software decoder");
        for decoder in [
            " -hwaccel cuda",
            " -hwaccel vaapi",
            " -hwaccel d3d11va",
            " -hwaccel videotoolbox",
        ] {
            assert!(is_hw_tonemap_available(&c, decoder), "{decoder}");
        }
        // QSV does not, so hardware tonemapping is refused.
        assert!(!is_hw_tonemap_available(&c, " -hwaccel qsv"));
    }

    #[test]
    fn rkmpp_reads_dolby_vision_rpus_only_from_ffmpeg_7_1_1() {
        let build = |version: FfmpegVersion| {
            FfmpegCapabilities::builder()
                .platform(Platform::Linux)
                .hwaccels(["rkmpp", "opencl"])
                .filters(REQUIRED_FILTERS)
                .all_filter_options(true)
                .ffmpeg_version(version)
                .build()
        };
        let options = tonemap_options(HardwareAccelerationType::rkmpp);
        let mut s = hdr_stream();
        s.video_range_type = Some(VideoRangeType::Dovi);

        let new = build(FfmpegVersion::with_build(7, 1, 1));
        assert!(is_hw_tonemap_available(
            &ctx(&new, &options, &s),
            " -hwaccel rkmpp"
        ));
        let old = build(V701);
        assert!(!is_hw_tonemap_available(
            &ctx(&old, &options, &s),
            " -hwaccel rkmpp"
        ));
    }

    // ----- the branches mutation testing found unpinned ----------------------

    #[test]
    fn an_audio_only_request_gets_no_device_graph() {
        // C#'s very first line: `if (!state.IsVideoRequest) return string.Empty`.
        // Initialising a CUDA device for an audio job would fail the whole
        // command on a machine without the GPU.
        let caps = caps(Platform::Linux);
        let options = options(HardwareAccelerationType::nvenc);
        let s = stream("h264", "yuv420p");
        let out = input_video_hwaccel_args(
            &ctx(&caps, &options, &s),
            "h264_nvenc",
            node(),
            RenderNode::default(),
            false,
        );
        assert_eq!(out.args, "");
        assert!(out.env.is_empty());
    }

    #[test]
    fn an_accelerator_with_no_branch_emits_no_device_graph() {
        // V4L2M2M and "none" have no branch in the C# if-chain: they fall
        // through to the decoder tail rather than bailing. Their decoder is
        // always empty, so the *output* is the same either way — this pins that
        // neither emits a device graph, which is the observable half.
        let caps = caps(Platform::Linux);
        let s = stream("h264", "yuv420p");
        for hw_type in [
            HardwareAccelerationType::v4l2m2m,
            HardwareAccelerationType::none,
        ] {
            let options = options(hw_type);
            let out = args_for(&ctx(&caps, &options, &s), "h264_v4l2m2m", node());
            assert_eq!(out.args, "", "{hw_type:?}");
        }
    }

    #[test]
    fn qsv_tonemapping_derives_opencl_from_the_platforms_device() {
        let mut options = tonemap_options(HardwareAccelerationType::qsv);
        // Software decode, so the filter device moves to OpenCL.
        options.hardware_decoding_codecs = vec!["vc1".to_owned()];
        let s = hdr_stream();

        let linux = caps(Platform::Linux);
        let out = args_for(&ctx(&linux, &options, &s), "hevc_qsv", node());
        assert_eq!(
            out.args,
            "-init_hw_device vaapi=va:,vendor_id=0x8086,driver=iHD \
             -init_hw_device qsv=qs@va -init_hw_device opencl=ocl@va -filter_hw_device ocl"
        );

        // On Windows the OpenCL device derives from d3d11va instead.
        let windows = caps(Platform::Windows);
        let out = args_for(&ctx(&windows, &options, &s), "hevc_qsv", node());
        assert_eq!(
            out.args,
            "-init_hw_device d3d11va=dx11:,vendor=0x8086 -init_hw_device qsv=qs@dx11 \
             -init_hw_device opencl=ocl@dx11 -filter_hw_device ocl"
        );
    }

    #[test]
    fn qsv_keeps_the_qsv_filter_device_when_the_decode_is_hardware() {
        // With a hardware decoder the frames are already on the GPU, so the
        // filter device stays `qs` even though an OpenCL device is added.
        let caps = caps(Platform::Linux);
        let mut options = tonemap_options(HardwareAccelerationType::qsv);
        options.hardware_decoding_codecs = vec!["hevc".to_owned()];
        let s = hdr_stream();
        let out = args_for(&ctx(&caps, &options, &s), "hevc_qsv", node());
        assert!(
            out.args.contains("-init_hw_device opencl=ocl@va"),
            "{}",
            out.args
        );
        assert!(out.args.contains("-filter_hw_device qs"), "{}", out.args);
        assert!(!out.args.contains("-filter_hw_device ocl"), "{}", out.args);
    }

    #[test]
    fn qsv_adds_no_opencl_device_on_a_build_without_the_parent_hwaccels() {
        // The guard C# writes as `SupportsHwaccel("vaapi") || SupportsHwaccel("d3d11va")`
        // — the OpenCL device is derived from one of them, so without either
        // there is nothing to derive from.
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["qsv", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .build();
        let mut options = tonemap_options(HardwareAccelerationType::qsv);
        options.hardware_decoding_codecs = vec!["vc1".to_owned()];
        let s = hdr_stream();
        let out = args_for(&ctx(&caps, &options, &s), "hevc_qsv", node());
        assert!(!out.args.contains("opencl"), "{}", out.args);
        assert!(out.args.contains("-filter_hw_device qs"), "{}", out.args);
    }

    #[test]
    fn vaapi_adds_no_opencl_device_when_the_decode_is_already_vaapi() {
        // The `&& !isVaapiDecoder` half of the Intel tonemap branch: frames
        // decoded into VAAPI surfaces need no separate upload device.
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS)
            .decoders(REQUIRED_DECODERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .vaapi_driver(false, true, false)
            .build();
        let mut options = tonemap_options(HardwareAccelerationType::vaapi);
        options.hardware_decoding_codecs = vec!["hevc".to_owned()];
        let s = hdr_stream();
        let out = args_for(&ctx(&caps, &options, &s), "hevc_vaapi", node());
        assert!(!out.args.contains("opencl"), "{}", out.args);
    }

    #[test]
    fn rkmpp_adds_no_opencl_device_when_the_decode_is_already_rkmpp() {
        let caps = caps(Platform::Linux);
        let mut options = tonemap_options(HardwareAccelerationType::rkmpp);
        options.hardware_decoding_codecs = vec!["hevc".to_owned()];
        let mut s = hdr_stream();
        s.width = Some(1920);
        s.height = Some(1080);
        let mut c = ctx(&caps, &options, &s);
        c.requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };
        let out = args_for(&c, "hevc_rkmpp", node());
        assert!(!out.args.contains("opencl"), "{}", out.args);

        // Software decode instead: the OpenCL device appears and takes the
        // filter device with it.
        let mut sw = options.clone();
        sw.hardware_decoding_codecs = vec!["vc1".to_owned()];
        let mut c = ctx(&caps, &sw, &s);
        c.requested = RequestedSize {
            width: Some(1920),
            height: Some(1080),
            ..RequestedSize::default()
        };
        let out = args_for(&c, "hevc_rkmpp", node());
        assert_eq!(
            out.args,
            "-init_hw_device rkmpp=rk -init_hw_device opencl=ocl@rk -filter_hw_device ocl"
        );
    }

    #[test]
    fn amf_without_full_opencl_gets_the_d3d11_device_and_no_filter_device() {
        let caps = FfmpegCapabilities::builder()
            .platform(Platform::Windows)
            .hwaccels(["d3d11va"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(V701)
            .build();
        let options = options(HardwareAccelerationType::amf);
        let s = stream("h264", "yuv420p");
        let out = args_for(&ctx(&caps, &options, &s), "h264_amf", node());
        assert_eq!(
            out.args,
            "-init_hw_device d3d11va=dx11:,vendor=0x1002 -hwaccel d3d11va -threads 2"
        );
    }

    // ----- the graphical subtitle canvas -------------------------------------

    #[test]
    fn a_bitmap_subtitle_gets_a_canvas_sized_to_itself() {
        let sub = MediaStream {
            codec: Some("PGSSUB".to_owned()),
            is_text_subtitle_stream: Some(false),
            width: Some(1920),
            height: Some(1080),
            ..MediaStream::default()
        };
        assert_eq!(
            graphical_sub_canvas_size(Some(&sub), true),
            " -canvas_size 1920x1080"
        );
        // Not burning it in means no canvas.
        assert_eq!(graphical_sub_canvas_size(Some(&sub), false), "");
        assert_eq!(graphical_sub_canvas_size(None, true), "");
    }

    #[test]
    fn text_subtitles_and_dvbsub_get_no_canvas() {
        // Text subtitles are drawn by the `subtitles` filter, which sizes
        // itself.
        let text = MediaStream {
            codec: Some("subrip".to_owned()),
            is_text_subtitle_stream: Some(true),
            width: Some(1920),
            height: Some(1080),
            ..MediaStream::default()
        };
        assert_eq!(graphical_sub_canvas_size(Some(&text), true), "");

        // DVBSUB is always 720x576 and carries no dimensions of its own.
        let dvb = MediaStream {
            codec: Some("DVBSUB".to_owned()),
            is_text_subtitle_stream: Some(false),
            width: Some(720),
            height: Some(576),
            ..MediaStream::default()
        };
        assert_eq!(graphical_sub_canvas_size(Some(&dvb), true), "");

        // A bitmap subtitle with no usable size gets nothing either.
        let sizeless = MediaStream {
            codec: Some("PGSSUB".to_owned()),
            is_text_subtitle_stream: Some(false),
            ..MediaStream::default()
        };
        assert_eq!(graphical_sub_canvas_size(Some(&sizeless), true), "");
    }
}
