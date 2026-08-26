//! The VAAPI filter chains.
//!
//! Port of C# `EncodingHelper.GetVaapiVidFilterChain` (10.11.z 4936-4996),
//! `GetIntelVaapiFullVidFiltersPrefered` (4997-5234) and
//! `GetVaapiLimitedVidFiltersPrefered` (5472-5680).
//!
//! **VAAPI is three pipelines, not one, and which you get is decided by the
//! driver behind the render node** — probed at runtime, not configured:
//!
//! | driver | chain | why |
//! |---|---|---|
//! | Intel iHD | [`intel_vaapi_full_vid_filters_prefered`] | has VPP tonemap and `overlay_vaapi` |
//! | AMD + Vulkan interop | the libplacebo path | tonemaps and scales in one Vulkan filter |
//! | Intel i965, legacy AMD | [`vaapi_limited_vid_filters_prefered`] | scale and deinterlace only |
//!
//! The AMD path is the work item of `PLAN_HWACCEL.md` phase 4c; until it lands
//! [`vaapi_vid_filter_chain`] routes an AMD device to the limited chain, which
//! is where upstream sends it anyway whenever the Vulkan preconditions fail.
//!
//! **That is not free.** Most of what the limited chain loses is performance —
//! subtitles composite in system memory instead of `overlay_vulkan`, and the
//! tonemap round-trips — but **a rotated video decoded in hardware comes out
//! sideways**: upstream's Vulkan chain transposes with `transpose_vulkan` and
//! swaps the dimensions, and the limited chain has no transpose at all and
//! swaps only for a software decode. Until 4c lands, an AMD user with a rotated
//! phone video and hardware decoding gets wrong output, not merely slow output.
//!
//! The two chains here differ in more than their filter names, and the
//! differences are the whole point of having both:
//!
//! - **Tonemapping.** iHD can tonemap in the VPP block (`tonemap_vaapi`) when
//!   the decode is on the GPU, and otherwise borrows OpenCL through
//!   Intel's zero-copy VAAPI↔OpenCL interop (`hwmap=derive_device=opencl`).
//!   The limited chain has no VPP tonemap at all, and only i965 has the
//!   interop — legacy AMD has to round-trip through system memory
//!   (`hwdownload` → `format=p010le` → `hwupload`).
//! - **Rotation.** Only the iHD chain uses `transpose_vaapi` — not because
//!   ffmpeg lacks the filter (the gate already required it) but because the
//!   older drivers do not expose the VPP entrypoint behind it. The limited
//!   chain therefore cannot rotate on the GPU, which is why its `swapWAndH` is
//!   a bare `|rotation| == 90 && isSwDecoder`: with no transpose to feed, a
//!   hardware decode has nothing to swap for.
//! - **Subtitles.** `overlay_vaapi` composites in VRAM, so iHD keeps the
//!   zero-copy path with a burned-in subtitle. The limited chain has no
//!   hardware overlay, so any subtitle forces the frames back to memory.

use std::fmt::Write as _;

use ferrofin_model::entities::HardwareAccelerationType;

use super::contains;
use super::decoder::fixed_output_size;
use super::filters::{
    alpha_src_filter, graphical_sub_preprocess_filters, hw_deinterlace_filter, hw_scale_filter,
    sw_deinterlace_filter, video_transpose_direction,
};
use super::nvidia::{MAX_ASS_SUBTITLE_FRAMERATE, STATIC_SUBTITLE_FRAMERATE};
use super::sw_chain::{
    ChainInput, FilterChain, SubtitleOverlay, sw_scale_filter, sw_vid_filter_chain,
};
use super::tonemap::{hw_tonemap_filter, overwrite_color_properties_param};

/// The height a subtitle surface is generated at for `overlay_vaapi`.
///
/// Port of the literal `1080` upstream passes as `reqMaxH` for the subtitle
/// chain — and *only* that chain; the video keeps the client's own bound.
/// `overlay_vaapi` scales the overlay itself, so a smaller surface costs
/// nothing in quality and moves less across the bus. Verbatim upstream, not a
/// tuning knob.
pub const OVERLAY_VAAPI_SUB_MAX_HEIGHT: i32 = 1080;

/// Extra VAAPI frame-pool slots for the VPP scaler.
///
/// Port of the literal `:extra_hw_frames=24`. The VPP block holds references to
/// frames it has not finished with, and the default pool is sized for a plain
/// decode; without the slack the scaler stalls waiting for its own output.
pub const VAAPI_VPP_EXTRA_HW_FRAMES: i32 = 24;

/// The VAAPI filter chain, falling back to software when the pipeline cannot
/// run. Port of `GetVaapiVidFilterChain`.
///
/// The fallback is richer than the other vendors': a job that cannot use the
/// VAAPI *filters* may still be encoding with a VAAPI *encoder*, and that
/// encoder needs its frames in VRAM. So the software chain is built and then
/// `hwupload=derive_device=vaapi` is appended — to the overlay list when there
/// is one, because that is what produces the final frame, and to the main list
/// otherwise.
#[must_use]
pub fn vaapi_vid_filter_chain(input: &ChainInput<'_>) -> FilterChain {
    let caps = input.caps;
    if input.options.hardware_acceleration_type != HardwareAccelerationType::vaapi {
        return FilterChain::default();
    }

    let is_sw_decoder = input.is_sw_decoder();
    let is_sw_encoder = !contains(input.video_encoder, "vaapi");
    let vaapi_full = caps.platform().is_linux()
        && super::support::is_vaapi_supported(caps, input.source_codec)
        && super::support::is_vaapi_full_supported(caps);
    let vaapi_ocl = vaapi_full && super::support::is_opencl_full_supported(caps);

    // Legacy copy-back pipeline.
    if (is_sw_decoder && is_sw_encoder) || !vaapi_ocl || !caps.supports_filter("alphasrc") {
        let mut chain = sw_vid_filter_chain(input);
        if !is_sw_encoder {
            // The upload goes onto whichever list ends the graph.
            if chain.overlay.is_empty() {
                chain.main.push("hwupload=derive_device=vaapi".to_owned());
            } else {
                chain
                    .overlay
                    .push("hwupload=derive_device=vaapi".to_owned());
            }
        }
        return chain;
    }

    if caps.is_vaapi_device_intel_ihd() {
        return intel_vaapi_full_vid_filters_prefered(input);
    }

    // The AMD Vulkan/libplacebo pipeline is phase 4c. Until then an AMD device
    // takes the limited chain — the shape upstream itself emits whenever the
    // Vulkan preconditions fail. Mostly that costs only speed, but see the
    // module docs: a hardware-decoded ROTATED video loses its rotation here,
    // which is wrong output rather than slow output.
    vaapi_limited_vid_filters_prefered(input)
}

/// What the two chains read the same way, resolved once.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a separate fact the C# reads independently off the job; \
              grouping them would invent a taxonomy upstream does not have"
)]
struct VaapiJob<'a> {
    input: &'a ChainInput<'a>,
    is_vaapi_decoder: bool,
    is_vaapi_encoder: bool,
    is_sw_decoder: bool,
    is_mjpeg_encoder: bool,
    /// The zero-copy case: decode and encode both on the GPU.
    is_va_in_va_out: bool,
    deinterlace: bool,
    has_subs: bool,
    has_text_subs: bool,
    has_graphical_subs: bool,
}

impl<'a> VaapiJob<'a> {
    fn new(input: &'a ChainInput<'a>) -> Self {
        let is_vaapi_decoder = contains(input.video_decoder, "vaapi");
        let is_vaapi_encoder = contains(input.video_encoder, "vaapi");
        Self {
            input,
            is_vaapi_decoder,
            is_vaapi_encoder,
            is_sw_decoder: input.is_sw_decoder(),
            is_mjpeg_encoder: contains(input.video_encoder, "mjpeg"),
            is_va_in_va_out: is_vaapi_decoder && is_vaapi_encoder,
            deinterlace: input.deinterlace,
            has_subs: input.subtitle.is_some(),
            has_text_subs: matches!(input.subtitle, SubtitleOverlay::Text { .. }),
            has_graphical_subs: matches!(input.subtitle, SubtitleOverlay::Graphical { .. }),
        }
    }
}

/// Appends the VPP scaler's trailing options. Port of the `isMjpegEncoder` and
/// `extra_hw_frames` tails both chains share.
///
/// Both only apply to a scaler that exists: upstream tests
/// `!string.IsNullOrEmpty(hwScaleFilter)` first, and appending options to an
/// empty string would emit a bare `:extra_hw_frames=24` as if it were a filter.
fn with_vpp_scaler_options(
    mut filter: String,
    is_mjpeg_encoder: bool,
    do_ocl_tonemap: bool,
) -> String {
    if filter.is_empty() {
        return filter;
    }
    if is_mjpeg_encoder {
        // MJPEG carries no range flag, so the scaler has to produce full range
        // — unless the tonemapper downstream will set it instead.
        if !do_ocl_tonemap {
            filter.push_str(":out_range=pc");
        }
        filter.push_str(":mode=hq");
    }
    let _ = write!(filter, ":extra_hw_frames={VAAPI_VPP_EXTRA_HW_FRAMES}");
    filter
}

/// The software scaler with MJPEG's full-range tail. Port of the shared
/// `isMjpegEncoder && !doOclTonemap` branch of the sw-decode block.
///
/// Note it produces a bare `scale=out_range=pc` when there is nothing to
/// scale — upstream wants the range set even with no resize.
fn sw_scale_with_mjpeg_range(
    job: &VaapiJob<'_>,
    in_w: Option<i32>,
    in_h: Option<i32>,
    do_ocl_tonemap: bool,
) -> String {
    let input = job.input;
    let filter = sw_scale_filter(
        input.video_encoder,
        in_w,
        in_h,
        input.three_d_format,
        input.requested,
    );
    if !job.is_mjpeg_encoder || do_ocl_tonemap {
        return filter;
    }
    if filter.is_empty() {
        "scale=out_range=pc".to_owned()
    } else {
        format!("{filter}:out_range=pc")
    }
}

/// The Intel iHD pipeline. Port of `GetIntelVaapiFullVidFiltersPrefered`.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear pipeline in ffmpeg's own filter order; splitting it \
              would scatter the sequence across helpers that all re-thread the \
              same frame-location state"
)]
pub fn intel_vaapi_full_vid_filters_prefered(input: &ChainInput<'_>) -> FilterChain {
    let job = VaapiJob::new(input);
    let caps = input.caps;
    let options = input.options;

    // VPP tonemapping needs the frames already on the GPU; OpenCL is the
    // fallback, and the two are mutually exclusive.
    let do_va_vpp_tonemap = job.is_vaapi_decoder && input.vpp_tonemap_available;
    let do_ocl_tonemap = !do_va_vpp_tonemap && input.do_hw_tonemap;
    let do_tonemap = do_va_vpp_tonemap || do_ocl_tonemap;

    let rotation = input.rotation.unwrap_or(0);
    let transpose_dir = if rotation == 0 {
        ""
    } else {
        video_transpose_direction(input.rotation)
    };
    // Unlike CUDA, no capability test: `transpose_vaapi` is part of the VPP
    // block that `IsVaapiFullSupported` already required.
    let do_va_vpp_transpose = !transpose_dir.is_empty();
    let swap = rotation.abs() == 90
        && (job.is_sw_decoder || (job.is_vaapi_decoder && do_va_vpp_transpose));
    let (in_w, in_h) = if swap {
        (input.video_height, input.video_width)
    } else {
        (input.video_width, input.video_height)
    };

    let mut chain = FilterChain::default();
    chain.main.push(overwrite_color_properties_param(
        input.color_transfer,
        do_tonemap,
    ));

    if job.is_sw_decoder {
        if job.deinterlace {
            chain
                .main
                .push(sw_deinterlace_filter(options, input.reference_frame_rate));
        }
        // The extra bits only matter if OpenCL will tonemap; otherwise the
        // VAAPI encoder wants nv12 anyway.
        let out_format = if do_ocl_tonemap {
            "yuv420p10le"
        } else {
            "nv12"
        };
        chain
            .main
            .push(sw_scale_with_mjpeg_range(&job, in_w, in_h, do_ocl_tonemap));
        chain.main.push(format!("format={out_format}"));
        if do_ocl_tonemap {
            // The only reason to move CPU frames onto the GPU here: a hwupload
            // costs more than running the rest of the chain in software.
            chain.main.push("hwupload=derive_device=opencl".to_owned());
        }
    } else if job.is_vaapi_decoder {
        if job.deinterlace {
            chain.main.push(hw_deinterlace_filter(
                caps,
                options,
                input.reference_frame_rate,
                "vaapi",
            ));
        }
        if do_va_vpp_transpose {
            chain
                .main
                .push(format!("transpose_vaapi=dir={transpose_dir}"));
        }
        // HEVC Range Extensions decode to a 10-bit surface, so the scaler has
        // to be told p010 before the tonemapper reads it.
        let out_format = if do_tonemap {
            if input.is_hevc_rext { "p010" } else { "" }
        } else {
            "nv12"
        };
        let scale = hw_scale_filter(
            "scale",
            "vaapi",
            Some(out_format),
            false,
            in_w,
            in_h,
            input.requested,
        );
        chain.main.push(with_vpp_scaler_options(
            scale,
            job.is_mjpeg_encoder,
            do_ocl_tonemap,
        ));
    }

    if do_va_vpp_tonemap && job.is_vaapi_decoder {
        chain.main.push(hw_tonemap_filter(
            caps,
            options,
            "vaapi",
            Some("nv12"),
            job.is_mjpeg_encoder,
        ));
    }

    if do_ocl_tonemap && job.is_vaapi_decoder {
        // Intel's zero-copy VAAPI↔OpenCL interop: the frame never leaves VRAM,
        // it is only reinterpreted as an OpenCL image.
        chain
            .main
            .push("hwmap=derive_device=opencl:mode=read".to_owned());
    }

    if do_ocl_tonemap {
        chain.main.push(hw_tonemap_filter(
            caps,
            options,
            "opencl",
            Some("nv12"),
            job.is_mjpeg_encoder,
        ));
    }

    if do_ocl_tonemap && job.is_va_in_va_out {
        // ...and back the same way, so the encoder sees a VAAPI surface.
        chain
            .main
            .push("hwmap=derive_device=vaapi:mode=write:reverse=1".to_owned());
        chain.main.push("format=vaapi".to_owned());
    }

    let mut memory_output = false;
    let is_upload_for_ocl_tonemap = job.is_sw_decoder && do_ocl_tonemap;
    // A frame uploaded from memory for OpenCL has no VAAPI surface to map back
    // onto, so it has to be copied rather than remapped.
    let is_hwmap_not_usable = is_upload_for_ocl_tonemap && job.is_vaapi_encoder;
    if (job.is_vaapi_decoder && !job.is_vaapi_encoder) || is_upload_for_ocl_tonemap {
        memory_output = true;
        chain.main.push(
            if is_hwmap_not_usable {
                "hwdownload"
            } else {
                "hwmap=mode=read"
            }
            .to_owned(),
        );
        chain.main.push("format=nv12".to_owned());
    }

    if job.is_sw_decoder && job.is_vaapi_encoder {
        memory_output = true;
    }

    if memory_output && let SubtitleOverlay::Text { plain, .. } = input.subtitle {
        chain.main.push(plain.to_owned());
    }

    // A graphical subtitle uploads after its overlay instead, so the main
    // chain must not upload ahead of it.
    if memory_output && job.is_vaapi_encoder && !job.has_graphical_subs {
        chain.main.push("hwupload_vaapi".to_owned());
    }

    if job.is_va_in_va_out {
        if job.has_subs {
            // The subtitle surface is generated at a reduced height because
            // `overlay_vaapi` scales it on the GPU.
            let sub_requested = input
                .requested
                .with_max_height(OVERLAY_VAAPI_SUB_MAX_HEIGHT);
            match input.subtitle {
                SubtitleOverlay::Graphical { width, height } => {
                    chain.sub.push(graphical_sub_preprocess_filters(
                        in_w,
                        in_h,
                        width,
                        height,
                        sub_requested,
                    ));
                    chain.sub.push("format=bgra".to_owned());
                }
                SubtitleOverlay::Text {
                    alpha_sub2video,
                    is_ass,
                    ..
                } => {
                    let sub_framerate = if is_ass {
                        input
                            .real_frame_rate
                            .unwrap_or(25.0)
                            .min(MAX_ASS_SUBTITLE_FRAMERATE)
                    } else {
                        STATIC_SUBTITLE_FRAMERATE
                    };
                    chain.sub.push(alpha_src_filter(
                        in_w,
                        in_h,
                        sub_requested,
                        Some(sub_framerate),
                        input.start_time_ticks,
                    ));
                    chain.sub.push("format=bgra".to_owned());
                    chain.sub.push(alpha_sub2video.to_owned());
                }
                SubtitleOverlay::None => {}
            }
            chain.sub.push("hwupload=derive_device=vaapi".to_owned());

            // `overlay_vaapi` is told the output size so it can scale the
            // reduced-height surface back up as it composites.
            let (overlay_w, overlay_h) = fixed_output_size(in_w, in_h, input.requested);
            let overlay_size = match (overlay_w, overlay_h) {
                (Some(w), Some(h)) => format!(":w={w}:h={h}"),
                _ => String::new(),
            };
            chain.overlay.push(format!(
                "overlay_vaapi=eof_action=pass:repeatlast=0{overlay_size}"
            ));
        }
    } else if memory_output && let SubtitleOverlay::Graphical { width, height } = input.subtitle {
        chain.sub.push(graphical_sub_preprocess_filters(
            in_w,
            in_h,
            width,
            height,
            input.requested,
        ));
        chain
            .overlay
            .push("overlay=eof_action=pass:repeatlast=0".to_owned());
        if job.is_vaapi_encoder {
            chain.overlay.push("hwupload_vaapi".to_owned());
        }
    }

    chain
}

/// The i965 / legacy-AMD pipeline. Port of `GetVaapiLimitedVidFiltersPrefered`.
///
/// "Limited" is the driver's capability, not a policy, and not ffmpeg's: the
/// gate already proved ffmpeg has the VPP filters. These drivers do not expose
/// the entrypoints behind them — no VPP tonemap, no transpose, no overlay — so
/// anything past scale and deinterlace has to leave the VAAPI surface.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "same linear-pipeline rationale as the iHD chain above"
)]
pub fn vaapi_limited_vid_filters_prefered(input: &ChainInput<'_>) -> FilterChain {
    let job = VaapiJob::new(input);
    let caps = input.caps;
    let options = input.options;
    let is_i965 = caps.is_vaapi_device_intel_i965();

    let do_ocl_tonemap = input.do_hw_tonemap;

    let rotation = input.rotation.unwrap_or(0);
    // No `transpose_vaapi` on these drivers, so a hardware decode has nothing
    // to swap dimensions *for* — unlike the iHD chain, this is sw-decode only.
    let swap = rotation.abs() == 90 && job.is_sw_decoder;
    let (in_w, in_h) = if swap {
        (input.video_height, input.video_width)
    } else {
        (input.video_width, input.video_height)
    };

    let mut chain = FilterChain::default();
    chain.main.push(overwrite_color_properties_param(
        input.color_transfer,
        do_ocl_tonemap,
    ));

    if job.is_sw_decoder {
        if job.deinterlace {
            chain
                .main
                .push(sw_deinterlace_filter(options, input.reference_frame_rate));
        }
        let out_format = if do_ocl_tonemap {
            "yuv420p10le"
        } else {
            "nv12"
        };
        chain
            .main
            .push(sw_scale_with_mjpeg_range(&job, in_w, in_h, do_ocl_tonemap));
        chain.main.push(format!("format={out_format}"));
        if do_ocl_tonemap {
            chain.main.push("hwupload=derive_device=opencl".to_owned());
        }
    } else if job.is_vaapi_decoder {
        if job.deinterlace {
            chain.main.push(hw_deinterlace_filter(
                caps,
                options,
                input.reference_frame_rate,
                "vaapi",
            ));
        }
        let out_format = if do_ocl_tonemap { "" } else { "nv12" };
        // NOTE: the UNSWAPPED dimensions, deliberately. This chain cannot
        // rotate, so `swap` above is false for any hardware decode anyway —
        // but upstream passes `inW`/`inH` here explicitly rather than the
        // swapped pair, and the two differ if that ever stops holding.
        let scale = hw_scale_filter(
            "scale",
            "vaapi",
            Some(out_format),
            false,
            input.video_width,
            input.video_height,
            input.requested,
        );
        chain.main.push(with_vpp_scaler_options(
            scale,
            job.is_mjpeg_encoder,
            do_ocl_tonemap,
        ));
    }

    if do_ocl_tonemap && job.is_vaapi_decoder {
        if is_i965 {
            // i965 has the Intel interop, so the frame is remapped in place.
            chain.main.push("hwmap=derive_device=opencl".to_owned());
        } else {
            // Legacy AMD has none: the frame goes out to memory and back.
            chain.main.push("hwdownload".to_owned());
            chain.main.push("format=p010le".to_owned());
            chain.main.push("hwupload=derive_device=opencl".to_owned());
        }
    }

    if do_ocl_tonemap {
        chain.main.push(hw_tonemap_filter(
            caps,
            options,
            "opencl",
            Some("nv12"),
            job.is_mjpeg_encoder,
        ));
    }

    if do_ocl_tonemap && job.is_va_in_va_out && is_i965 {
        chain
            .main
            .push("hwmap=derive_device=vaapi:reverse=1".to_owned());
        chain.main.push("format=vaapi".to_owned());
    }

    let mut memory_output = false;
    let is_upload_for_ocl_tonemap =
        do_ocl_tonemap && (job.is_sw_decoder || (job.is_vaapi_decoder && !is_i965));
    let is_hwmap_not_usable = job.has_graphical_subs || is_upload_for_ocl_tonemap;
    // Any subtitle at all forces a hardware-decoded frame back to memory:
    // there is no hardware overlay on these drivers.
    let is_hwmap_for_subs = job.has_subs && job.is_vaapi_decoder;
    let is_hwunmap_for_text_subs =
        job.has_text_subs && job.is_va_in_va_out && !is_upload_for_ocl_tonemap;
    if (job.is_vaapi_decoder && !job.is_vaapi_encoder)
        || is_upload_for_ocl_tonemap
        || is_hwmap_for_subs
    {
        memory_output = true;
        chain.main.push(
            if is_hwmap_not_usable {
                "hwdownload"
            } else {
                "hwmap"
            }
            .to_owned(),
        );
        chain.main.push("format=nv12".to_owned());
    }

    if job.is_sw_decoder && job.is_vaapi_encoder {
        memory_output = true;
    }

    if memory_output && let SubtitleOverlay::Text { plain, .. } = input.subtitle {
        chain.main.push(plain.to_owned());
    }

    if is_hwunmap_for_text_subs {
        // The frames only came down to draw the text; map them straight back
        // rather than re-uploading a copy.
        chain.main.push("hwmap".to_owned());
        chain.main.push("format=vaapi".to_owned());
    } else if memory_output && job.is_vaapi_encoder && !job.has_graphical_subs {
        chain.main.push("hwupload_vaapi".to_owned());
    }

    if memory_output && let SubtitleOverlay::Graphical { width, height } = input.subtitle {
        chain.sub.push(graphical_sub_preprocess_filters(
            in_w,
            in_h,
            width,
            height,
            input.requested,
        ));
        chain
            .overlay
            .push("overlay=eof_action=pass:repeatlast=0".to_owned());
        if job.is_vaapi_encoder {
            chain.overlay.push("hwupload_vaapi".to_owned());
        }
    }

    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{FfmpegVersion, REQUIRED_FILTERS};
    use crate::encoding_helper::hw::capabilities::{FfmpegCapabilities, Platform};
    use crate::encoding_helper::hw::decoder::RequestedSize;
    use ferrofin_model::configuration::EncodingOptions;

    // Every golden here was derived from the C# (10.11.z 4936-5680) by a
    // transliteration written without reference to this file, then compared.
    // Upstream ships no tests for any of it.
    //
    // Two upstream defaults are load-bearing and easy to assume wrong:
    // `VppTonemappingBrightness` is **16**, not 0, so the VAAPI tonemap always
    // carries a `procamp_vaapi=b=16,` prefix at stock settings; and
    // `TonemappingMode` is `auto`, which is in neither of upstream's mode
    // lists, so no `:tonemap_mode=` is ever appended.

    const VAAPI_DECODER: &str = " -hwaccel vaapi -hwaccel_output_format vaapi";

    fn caps_for(driver: &str) -> FfmpegCapabilities {
        let b = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1));
        match driver {
            "ihd" => b.vaapi_driver(false, true, false),
            "i965" => b.vaapi_driver(false, false, true),
            "amd" => b.vaapi_driver(true, false, false),
            _ => b,
        }
        .build()
    }

    fn options() -> EncodingOptions {
        EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            ..EncodingOptions::default()
        }
    }

    fn input<'a>(
        caps: &'a FfmpegCapabilities,
        options: &'a EncodingOptions,
        decoder: &'a str,
        encoder: &'a str,
    ) -> ChainInput<'a> {
        ChainInput {
            caps,
            options,
            video_encoder: encoder,
            video_decoder: decoder,
            video_width: Some(1920),
            video_height: Some(1080),
            requested: RequestedSize::default(),
            three_d_format: None,
            rotation: None,
            color_transfer: None,
            reference_frame_rate: Some(25.0),
            real_frame_rate: Some(25.0),
            start_time_ticks: 0,
            deinterlace: false,
            do_sw_tonemap: false,
            do_hw_tonemap: false,
            vpp_tonemap_available: false,
            source_codec: Some("h264"),
            is_dovi: false,
            is_hevc_rext: false,
            subtitle: SubtitleOverlay::None,
        }
    }

    fn bounded(w: i32, h: i32) -> RequestedSize {
        RequestedSize {
            max_width: Some(w),
            max_height: Some(h),
            ..RequestedSize::default()
        }
    }

    const SDR_PARAMS: &str = "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";
    const HDR_PARAMS: &str =
        "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc";
    const OCL_TONEMAP: &str =
        "tonemap_opencl=format=nv12:p=bt709:t=bt709:m=bt709:tonemap=bt2390:peak=100:desat=0";

    // ----- Intel iHD ---------------------------------------------------------

    #[test]
    fn ihd_keeps_a_gpu_to_gpu_job_entirely_in_vram() {
        let caps = caps_for("ihd");
        let options = options();
        let chain = intel_vaapi_full_vid_filters_prefered(&input(
            &caps,
            &options,
            VAAPI_DECODER,
            "h264_vaapi",
        ));
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
            ]
        );
        assert!(chain.sub.is_empty() && chain.overlay.is_empty());
    }

    #[test]
    fn ihd_scales_on_the_gpu_when_the_client_bounds_the_size() {
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.requested = bounded(1280, 720);
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=w=1280:h=720:format=nv12:extra_hw_frames=24".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_uploads_a_software_decode_for_the_hardware_encoder() {
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_vaapi");
        inp.requested = bounded(1280, 720);
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale=trunc(min(max(iw\\,ih*a)\\,min(1280\\,720*a))/2)*2:\
                 trunc(min(max(iw/a\\,ih)\\,min(1280/a\\,720))/2)*2"
                    .replace(' ', ""),
                "format=nv12".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_maps_rather_than_copies_when_a_software_encoder_follows() {
        // `hwmap=mode=read` on a VAAPI surface is a reinterpretation, not a
        // copy — upstream prefers it to `hwdownload` wherever the frame really
        // is a VAAPI one.
        let caps = caps_for("ihd");
        let options = options();
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&input(
                &caps,
                &options,
                VAAPI_DECODER,
                "libx264"
            ))
            .main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
                "hwmap=mode=read".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_prefers_its_own_vpp_tonemap_over_opencl() {
        // The `procamp_vaapi=b=16,` prefix is upstream's DEFAULT, not an
        // opt-in: VppTonemappingBrightness ships as 16. And the whole thing is
        // one filter-list entry containing a comma, not two entries.
        let caps = caps_for("ihd");
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:\
                 extra_hw_frames=32"
                    .replace(' ', ""),
            ]
        );
    }

    #[test]
    fn ihd_borrows_opencl_through_zero_copy_interop_when_vpp_is_off() {
        // Intel can reinterpret a VAAPI surface as an OpenCL image in place, so
        // the tonemap costs no transfer — and the result maps straight back.
        let caps = caps_for("ihd");
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwmap=derive_device=opencl:mode=read".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwmap=derive_device=vaapi:mode=write:reverse=1".to_owned(),
                "format=vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_copies_back_when_the_frame_was_uploaded_rather_than_decoded() {
        // A frame uploaded from memory for OpenCL has no VAAPI surface to map
        // back onto, so this is the one iHD case that must copy.
        let caps = caps_for("ihd");
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, "", "h264_vaapi");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "format=yuv420p10le".to_owned(),
                "hwupload=derive_device=opencl".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwdownload".to_owned(),
                "format=nv12".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_composites_a_text_subtitle_on_the_gpu() {
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.requested = bounded(1280, 720);
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(
            chain.sub,
            vec![
                "alphasrc=s=1280x720:r=10:start='0'".to_owned(),
                "format=bgra".to_owned(),
                "subtitles=f='/media/a.srt':alpha=1:sub2video=1".to_owned(),
                "hwupload=derive_device=vaapi".to_owned(),
            ]
        );
        assert_eq!(
            chain.overlay,
            vec!["overlay_vaapi=eof_action=pass:repeatlast=0:w=1280:h=720"]
        );
        // The frames never came down, so the plain spelling is not in the main
        // chain.
        assert!(!chain.main.iter().any(|f| f.contains("subtitles=")));
    }

    #[test]
    fn ihd_generates_the_subtitle_plane_at_a_reduced_height() {
        // `overlay_vaapi` rescales the overlay itself, so upstream caps the
        // generated plane at 1080 to move less across the bus — while the
        // overlay's own `:w=:h=` still names the VIDEO's size. At 1080p the two
        // coincide; a 4K source is what separates them.
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.video_width = Some(3840);
        inp.video_height = Some(2160);
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(3840),
            height: Some(2160),
        };
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(chain.sub[0], "scale,scale=1920:1080:fast_bilinear");
        assert_eq!(
            chain.overlay,
            vec!["overlay_vaapi=eof_action=pass:repeatlast=0:w=3840:h=2160"]
        );
    }

    #[test]
    fn ihd_transposes_on_the_gpu_but_only_swaps_at_ninety_degrees() {
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.rotation = Some(90);
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main[1],
            "transpose_vaapi=dir=cclock"
        );

        // A half turn still transposes, but the frame keeps its shape.
        inp.rotation = Some(180);
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(chain.main[1], "transpose_vaapi=dir=reversal");
        assert_eq!(chain.sub[0], "alphasrc=s=1920x1080:r=10:start='0'");

        // ...where a quarter turn does swap it — and the swapped 1080x1920
        // frame then meets the subtitle plane's 1080 height cap, which scales
        // it by 1080/1920 to 608x1080. (Not 1080x1920: the cap applies to the
        // ROTATED shape, and 1920*0.5625 = 1080 leaves 1080*0.5625 = 607.5,
        // rounded to even.)
        inp.rotation = Some(90);
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(chain.sub[0], "alphasrc=s=608x1080:r=10:start='0'");
    }

    #[test]
    fn ihd_forces_full_range_for_an_mjpeg_encoder() {
        // MJPEG carries no range flag. With nothing to scale, upstream replaces
        // the empty scaler wholesale rather than appending to it.
        let caps = caps_for("ihd");
        let options = options();
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&input(&caps, &options, "", "mjpeg_vaapi")).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale=out_range=pc".to_owned(),
                "format=nv12".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_overlays_a_graphical_subtitle_in_memory_for_a_software_encoder() {
        // Not the `isVaInVaOut` branch: no `format=bgra`, no hwupload.
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "libx264");
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(chain.sub, vec!["scale,scale=1920:1080:fast_bilinear"]);
        assert_eq!(chain.overlay, vec!["overlay=eof_action=pass:repeatlast=0"]);
    }

    #[test]
    fn ihd_maps_back_for_a_software_encoder_even_after_an_upload() {
        // The counterpart to the copy-back case above. The frames were
        // uploaded for OpenCL, but with a SOFTWARE encoder there is no VAAPI
        // surface in play at all, so upstream still prefers the map.
        let caps = caps_for("ihd");
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, "", "libx264");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "format=yuv420p10le".to_owned(),
                "hwupload=derive_device=opencl".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwmap=mode=read".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
    }

    #[test]
    fn the_vpp_scaler_options_come_in_upstreams_order() {
        // Three separate appends, and the order is the contract: the mjpeg
        // range and quality first, then the pool size.
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "mjpeg_vaapi");
        inp.requested = bounded(1280, 720);
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main[1],
            "scale_vaapi=w=1280:h=720:format=nv12:out_range=pc:mode=hq:extra_hw_frames=24"
        );

        // With an OpenCL tonemap downstream the range is left to the
        // tonemapper, but `:mode=hq` still applies.
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options
        };
        let mut inp = input(&caps, &options, VAAPI_DECODER, "mjpeg_vaapi");
        inp.requested = bounded(1280, 720);
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main[1],
            "scale_vaapi=w=1280:h=720:mode=hq:extra_hw_frames=24"
        );
    }

    #[test]
    fn ihd_burns_text_in_memory_on_the_way_to_a_software_encoder() {
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "libx264");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
                "hwmap=mode=read".to_owned(),
                "format=nv12".to_owned(),
                "subtitles=f='/media/a.srt'".to_owned(),
            ]
        );
        assert!(chain.sub.is_empty() && chain.overlay.is_empty());
    }

    #[test]
    fn ihd_lets_the_overlay_do_the_upload_for_a_graphical_subtitle() {
        // The main chain deliberately skips `hwupload_vaapi` here so the
        // overlay can do it — the two halves of one decision, and only the
        // suppression half is obvious from the main chain.
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_vaapi");
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert!(
            !chain.main.iter().any(|f| f == "hwupload_vaapi"),
            "{:?}",
            chain.main
        );
        assert_eq!(
            chain.overlay,
            vec![
                "overlay=eof_action=pass:repeatlast=0".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn ihd_tracks_an_ass_subtitles_own_frame_rate() {
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: true,
        };
        inp.real_frame_rate = Some(23.976);
        assert!(
            intel_vaapi_full_vid_filters_prefered(&inp).sub[0].contains(":r=23.976:"),
            "{:?}",
            intel_vaapi_full_vid_filters_prefered(&inp).sub
        );
        // ...but never past 60.
        inp.real_frame_rate = Some(120.0);
        assert!(intel_vaapi_full_vid_filters_prefered(&inp).sub[0].contains(":r=60:"));
    }

    #[test]
    fn ihd_deinterlaces_on_the_gpu_before_it_transposes() {
        // VAAPI names the rate rather than flagging it, unlike yadif.
        let caps = caps_for("ihd");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.deinterlace = true;
        inp.rotation = Some(90);
        let chain = intel_vaapi_full_vid_filters_prefered(&inp);
        assert_eq!(chain.main[1], "deinterlace_vaapi=rate=frame");
        assert_eq!(chain.main[2], "transpose_vaapi=dir=cclock");

        // ...and on the CPU for a software decode.
        let mut inp = input(&caps, &options, "", "h264_vaapi");
        inp.deinterlace = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main[1],
            "yadif=0:-1:0"
        );
    }

    #[test]
    fn hevc_range_extensions_are_scaled_to_p010_before_tonemapping() {
        // RExt decodes to a 10-bit surface, so the scaler has to say so or the
        // tonemapper reads the wrong format.
        let caps = caps_for("ihd");
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.is_hevc_rext = true;
        assert_eq!(
            intel_vaapi_full_vid_filters_prefered(&inp).main[1],
            "scale_vaapi=format=p010:extra_hw_frames=24"
        );
    }

    // ----- i965 / legacy AMD -------------------------------------------------

    #[test]
    fn the_limited_chain_scales_and_stops() {
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.requested = bounded(1280, 720);
        assert_eq!(
            vaapi_limited_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=w=1280:h=720:format=nv12:extra_hw_frames=24".to_owned(),
            ]
        );
    }

    #[test]
    fn i965_has_the_interop_and_spells_hwmap_without_a_mode() {
        // The same round trip as iHD, but every `hwmap` here is bare — no
        // `:mode=read` / `:mode=write` anywhere in this chain.
        let caps = caps_for("i965");
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            vaapi_limited_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwmap=derive_device=opencl".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwmap=derive_device=vaapi:reverse=1".to_owned(),
                "format=vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn legacy_amd_round_trips_through_memory_twice() {
        // No VAAPI/OpenCL interop on this driver, so the frame goes out to
        // memory to reach OpenCL — and the same fact forces a second
        // `hwdownload` afterwards, because a frame that was uploaded cannot be
        // mapped back.
        let caps = caps_for("amd");
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            vaapi_limited_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwdownload".to_owned(),
                "format=p010le".to_owned(),
                "hwupload=derive_device=opencl".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwdownload".to_owned(),
                "format=nv12".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn the_limited_chain_maps_back_up_after_burning_text_in_memory() {
        // Any subtitle drags a hardware-decoded frame down to memory here —
        // there is no hardware overlay. Once the text is drawn the frame maps
        // straight back rather than being re-uploaded, and `hwupload_vaapi` is
        // suppressed because upstream chains it as an `else if`.
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = vaapi_limited_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
                "hwmap".to_owned(),
                "format=nv12".to_owned(),
                "subtitles=f='/media/a.srt'".to_owned(),
                "hwmap".to_owned(),
                "format=vaapi".to_owned(),
            ]
        );
        assert!(!chain.main.iter().any(|f| f == "hwupload_vaapi"));
        assert!(chain.sub.is_empty() && chain.overlay.is_empty());
    }

    #[test]
    fn a_graphical_subtitle_forces_a_copy_rather_than_a_map() {
        // `isHwmapNotUsable` means something different here than in the iHD
        // chain: a graphical subtitle alone is enough to force `hwdownload`.
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "libx264");
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = vaapi_limited_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
                "hwdownload".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
        assert_eq!(chain.sub, vec!["scale,scale=1920:1080:fast_bilinear"]);
        assert_eq!(chain.overlay, vec!["overlay=eof_action=pass:repeatlast=0"]);
    }

    #[test]
    fn the_limited_chain_drops_rotation_entirely() {
        // No `transpose_vaapi` on these drivers, and on a hardware decode the
        // dimensions do not even swap. Upstream simply loses the rotation here;
        // do not "fix" it.
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_vaapi");
        inp.rotation = Some(90);
        let chain = vaapi_limited_vid_filters_prefered(&inp);
        assert!(!chain.main.iter().any(|f| f.contains("transpose")));
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
            ]
        );
    }

    #[test]
    fn the_limited_chain_does_not_swap_for_a_hardware_decode() {
        // The swap only exists to feed a transpose, and this chain has none —
        // so on a hardware decode the subtitle plane keeps the source's shape.
        // A graphical subtitle is what makes the difference visible: swapping
        // would put a 16:9 bitmap onto a 9:16 frame and take the pad branch.
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "libx264");
        inp.rotation = Some(90);
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        assert_eq!(
            vaapi_limited_vid_filters_prefered(&inp).sub,
            vec!["scale,scale=1920:1080:fast_bilinear"]
        );
    }

    #[test]
    fn the_limited_chain_uploads_a_software_decode_for_the_hardware_encoder() {
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_vaapi");
        inp.requested = bounded(1280, 720);
        assert_eq!(
            vaapi_limited_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale=trunc(min(max(iw\\,ih*a)\\,min(1280\\,720*a))/2)*2:\
                 trunc(min(max(iw/a\\,ih)\\,min(1280/a\\,720))/2)*2"
                    .replace(' ', ""),
                "format=nv12".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn the_limited_chain_lets_the_overlay_upload_a_graphical_subtitle() {
        let caps = caps_for("i965");
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_vaapi");
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = vaapi_limited_vid_filters_prefered(&inp);
        assert!(
            !chain.main.iter().any(|f| f == "hwupload_vaapi"),
            "{:?}",
            chain.main
        );
        assert_eq!(
            chain.overlay,
            vec![
                "overlay=eof_action=pass:repeatlast=0".to_owned(),
                "hwupload_vaapi".to_owned(),
            ]
        );
    }

    // ----- the gate ----------------------------------------------------------

    #[test]
    fn a_job_touching_no_gpu_takes_the_software_chain_unchanged() {
        let caps = caps_for("ihd");
        let options = options();
        assert_eq!(
            vaapi_vid_filter_chain(&input(&caps, &options, "", "libx264")).main,
            vec![
                SDR_PARAMS.to_owned(),
                String::new(),
                "format=yuv420p".to_owned()
            ]
        );
    }

    #[test]
    fn a_build_without_alphasrc_copies_back_into_the_vaapi_encoder() {
        // The chain cannot run, but the ENCODER still needs its frames in VRAM,
        // so the software chain is built and then uploaded from.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS.into_iter().filter(|f| *f != "alphasrc"))
            .all_filter_options(true)
            .vaapi_driver(false, true, false)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options();
        let mut inp = input(&without, &options, "", "h264_vaapi");
        inp.requested = RequestedSize {
            max_width: Some(1280),
            ..RequestedSize::default()
        };
        assert_eq!(
            vaapi_vid_filter_chain(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale=trunc(min(max(iw\\,ih*a)\\,1280)/2)*2:trunc(ow/a/2)*2".to_owned(),
                "format=nv12".to_owned(),
                "hwupload=derive_device=vaapi".to_owned(),
            ]
        );
    }

    #[test]
    fn the_copy_back_upload_follows_the_overlay_when_there_is_one() {
        // The upload has to end the graph, and with a subtitle the overlay is
        // what produces the final frame — so the main list is left untouched.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "drm", "opencl"])
            .filters(REQUIRED_FILTERS.into_iter().filter(|f| *f != "alphasrc"))
            .all_filter_options(true)
            .vaapi_driver(false, true, false)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options();
        let mut inp = input(&without, &options, "", "h264_vaapi");
        inp.requested = RequestedSize {
            max_width: Some(1280),
            ..RequestedSize::default()
        };
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = vaapi_vid_filter_chain(&inp);
        assert_eq!(
            chain.overlay,
            vec![
                "overlay=eof_action=pass:repeatlast=0".to_owned(),
                "hwupload=derive_device=vaapi".to_owned(),
            ]
        );
        assert!(!chain.main.iter().any(|f| f.contains("hwupload")));
    }

    #[test]
    fn a_different_accelerator_yields_nothing_at_all() {
        let caps = caps_for("ihd");
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
            ..EncodingOptions::default()
        };
        assert_eq!(
            vaapi_vid_filter_chain(&input(&caps, &options, VAAPI_DECODER, "h264_nvenc")),
            FilterChain::default()
        );
    }

    #[test]
    fn an_ihd_device_reaches_the_intel_chain_through_the_gate() {
        // The two chains agree on most jobs, so the discriminator has to be a
        // spelling only one of them uses: iHD maps with an explicit mode.
        let caps = caps_for("ihd");
        let options = options();
        let chain = vaapi_vid_filter_chain(&input(&caps, &options, VAAPI_DECODER, "libx264"));
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
                "hwmap=mode=read".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
    }

    #[test]
    fn an_amd_device_takes_the_limited_chain_until_its_own_lands() {
        // Phase 4c adds the Vulkan/libplacebo path. Until then AMD gets the
        // limited chain — the shape upstream emits whenever the Vulkan
        // preconditions fail. Note this is not purely a capability narrowing:
        // a hardware-decoded rotated video loses its rotation until 4c.
        let caps = caps_for("amd");
        let options = options();
        let chain = vaapi_vid_filter_chain(&input(&caps, &options, VAAPI_DECODER, "h264_vaapi"));
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=format=nv12:extra_hw_frames=24".to_owned(),
            ]
        );
    }
}
