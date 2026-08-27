//! The Quick Sync (QSV) filter chains.
//!
//! Port of C# `EncodingHelper.GetIntelVidFilterChain` (10.11.z 4324-4369) and
//! `GetIntelQsvVaapiVidFiltersPrefered` (4664-4935).
//!
//! **QSV is not a device, it is a layer over one.** On Linux it sits on VAAPI,
//! on Windows on D3D11, and which of those is underneath decides the whole
//! filter vocabulary — hence two chains rather than one with flags. The Windows
//! variant (`GetIntelQsvDx11VidFiltersPrefered`) is the work item of
//! `PLAN_HWACCEL.md` phase 5b.
//!
//! Because QSV sits on VAAPI here, a job can arrive decoded by **either**, and
//! the chain has to cope with both in the same pass:
//!
//! - A **VAAPI** decode scales with `scale_vaapi` and transposes with a
//!   separate `transpose_vaapi`.
//! - A **QSV** decode scales with `vpp_qsv` and transposes *inside* that same
//!   filter, via a `:transpose=` option — which is why the scaler needs to be
//!   told to swap its own output dimensions rather than being handed
//!   pre-swapped ones.
//!
//! Tonemapping always happens on the VAAPI side, so a QSV-decoded frame is
//! mapped down to VAAPI and back again around it. That mapping is free — both
//! are views of the same Intel surface — which is what makes the whole
//! arrangement worth it.

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
use super::vaapi::OVERLAY_VAAPI_SUB_MAX_HEIGHT;
use super::versions::{MIN_FFMPEG_QSV_VPP_OUT_RANGE_OPTION, MIN_FFMPEG_QSV_VPP_SCALE_MODE_OPTION};

/// The frame-pool size a QSV subtitle upload must ask for.
///
/// Port of the literal `64` in `hwupload=derive_device=qsv:extra_hw_frames=64`.
/// Upstream's comment is explicit that QSV needs a *fixed* pool and that
/// smaller values fail outright on some iGPUs — so this is a compatibility
/// floor, not a tuning knob.
pub const QSV_SUBTITLE_POOL_FRAMES: i32 = 64;

/// Extra frames for the reverse map out of OpenCL back into QSV.
///
/// Port of the literal `16`. Upstream's comment: without the slack `hevc_qsv`
/// fails with "cannot allocate memory".
pub const QSV_REVERSE_MAP_EXTRA_FRAMES: i32 = 16;

/// Extra frames for the VAAPI VPP scaler, as in the VAAPI chains.
pub const VAAPI_VPP_EXTRA_HW_FRAMES: i32 = 24;

/// The QSV filter chain, falling back to software when the pipeline cannot
/// run. Port of `GetIntelVidFilterChain`.
///
/// The fallback is plainer than VAAPI's: the software chain is returned
/// untouched, with no upload appended. QSV encoders take system-memory frames
/// directly, so there is nothing to hand up to the GPU.
#[must_use]
pub fn intel_vid_filter_chain(input: &ChainInput<'_>) -> FilterChain {
    let caps = input.caps;
    if input.options.hardware_acceleration_type != HardwareAccelerationType::qsv {
        return FilterChain::default();
    }

    let is_sw_decoder = input.is_sw_decoder();
    let is_sw_encoder = !contains(input.video_encoder, "qsv");
    let qsv_ocl = caps.supports_hwaccel("qsv") && super::support::is_opencl_full_supported(caps);
    let dx11_ocl = caps.platform().is_windows() && caps.supports_hwaccel("d3d11va") && qsv_ocl;
    let vaapi_ocl = caps.platform().is_linux()
        && super::support::is_vaapi_supported(caps, input.source_codec)
        && qsv_ocl;

    // Legacy copy-back pipeline.
    if (is_sw_decoder && is_sw_encoder)
        || (!vaapi_ocl && !dx11_ocl)
        || !caps.supports_filter("alphasrc")
    {
        return sw_vid_filter_chain(input);
    }

    if vaapi_ocl {
        return intel_qsv_vaapi_vid_filters_prefered(input);
    }

    // Windows, where QSV sits on D3D11 instead.
    intel_qsv_dx11_vid_filters_prefered(input)
}

/// The Windows QSV pipeline, layered on D3D11. Port of
/// `GetIntelQsvDx11VidFiltersPrefered`.
///
/// The same silicon as the Linux chain, reached a different way, and the
/// difference that matters is **where the tonemap happens**. On Linux QSV
/// borrows VAAPI's `tonemap_vaapi`; here it tonemaps inside `vpp_qsv` itself
/// via a `:tonemap=1` option — so there is no hop to another API at all.
///
/// That option cannot be combined with everything, which is where the two-pass
/// form comes from: procamp (brightness/contrast) and HEVC Range Extensions
/// each force the tonemap into a **second** `vpp_qsv`, with the first pass
/// producing `p010` for it to consume. Since `VppTonemappingBrightness`
/// defaults to 16, the two-pass form is what a stock Windows server actually
/// runs.
///
/// A `d3d11va` decode additionally has to be relayed into QSV — d3d11va has no
/// dynamic pool, so `:passthrough=0` forces the VPP filter to allocate its own
/// frames rather than letting encoder look-ahead exhaust the decoder's.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear pipeline in ffmpeg's own filter order; splitting it \
              would scatter the sequence across helpers that all re-thread the \
              same frame-location state"
)]
#[allow(
    clippy::similar_names,
    reason = "`is_sw_decoder` and `is_sw_encoder` are upstream's own names for \
              the two ends of the pipeline"
)]
pub fn intel_qsv_dx11_vid_filters_prefered(input: &ChainInput<'_>) -> FilterChain {
    let caps = input.caps;
    let options = input.options;

    let is_d3d11va_decoder = contains(input.video_decoder, "d3d11va");
    let is_qsv_decoder = contains(input.video_decoder, "qsv");
    let is_qsv_encoder = contains(input.video_encoder, "qsv");
    let is_hw_decoder = is_d3d11va_decoder || is_qsv_decoder;
    let is_sw_decoder = input.is_sw_decoder();
    let is_sw_encoder = !is_qsv_encoder;
    let is_mjpeg_encoder = contains(input.video_encoder, "mjpeg");
    let is_qsv_in_qsv_out = is_hw_decoder && is_qsv_encoder;

    let do_vpp_tonemap = input.vpp_tonemap_available;
    let do_ocl_tonemap = !do_vpp_tonemap && input.do_hw_tonemap;
    let do_tonemap = do_vpp_tonemap || do_ocl_tonemap;

    let has_graphical_subs = matches!(input.subtitle, SubtitleOverlay::Graphical { .. });

    let rotation = input.rotation.unwrap_or(0);
    let transpose_dir = if rotation == 0 {
        ""
    } else {
        video_transpose_direction(input.rotation)
    };
    let do_vpp_transpose = !transpose_dir.is_empty();
    let swap = rotation.abs() == 90 && (is_sw_decoder || (is_hw_decoder && do_vpp_transpose));
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

    if is_sw_decoder {
        if input.deinterlace {
            chain
                .main
                .push(sw_deinterlace_filter(options, input.reference_frame_rate));
        }
        let out_format = if do_ocl_tonemap {
            "yuv420p10le"
        } else if has_graphical_subs {
            "yuv420p"
        } else {
            "nv12"
        };
        let mut scale = sw_scale_filter(
            input.video_encoder,
            in_w,
            in_h,
            input.three_d_format,
            input.requested,
        );
        if is_mjpeg_encoder && !do_ocl_tonemap {
            scale = if scale.is_empty() {
                "scale=out_range=pc".to_owned()
            } else {
                format!("{scale}:out_range=pc")
            };
        }
        chain.main.push(scale);
        chain.main.push(format!("format={out_format}"));
        if do_ocl_tonemap {
            chain.main.push("hwupload=derive_device=opencl".to_owned());
        }
    } else if is_hw_decoder {
        let vpp_full_range_out = is_mjpeg_encoder
            && caps
                .ffmpeg_version()
                .is_some_and(|v| v >= MIN_FFMPEG_QSV_VPP_OUT_RANGE_OPTION);
        let vpp_scale_mode_hq = is_mjpeg_encoder
            && caps
                .ffmpeg_version()
                .is_some_and(|v| v >= MIN_FFMPEG_QSV_VPP_SCALE_MODE_OPTION);

        // Procamp and RExt each force the tonemap into its own second filter.
        // Brightness defaults to 16, so a stock server takes this path.
        let mut two_pass_tonemap = false;
        let mut procamp_params = String::new();
        if do_vpp_tonemap {
            if input.is_hevc_rext {
                // The VPP tonemap only consumes p010, so the first pass has to
                // produce it.
                two_pass_tonemap = true;
            }
            let brightness = options.vpp_tonemapping_brightness;
            let contrast = options.vpp_tonemapping_contrast;
            let mut do_procamp = false;
            if brightness != 0.0 && (-100.0..=100.0).contains(&brightness) {
                let _ = write!(procamp_params, ":brightness={brightness}");
                two_pass_tonemap = true;
                do_procamp = true;
            }
            if contrast > 1.0 && contrast <= 10.0 {
                let _ = write!(procamp_params, ":contrast={contrast}");
                two_pass_tonemap = true;
                do_procamp = true;
            }
            if do_procamp {
                procamp_params.push_str(":procamp=1:async_depth=2");
            } else {
                procamp_params.clear();
            }
        }

        let mut out_format = if do_ocl_tonemap {
            if do_vpp_transpose || input.is_hevc_rext {
                "p010"
            } else {
                ""
            }
        } else {
            "nv12"
        };
        if two_pass_tonemap {
            out_format = "p010";
        }

        // No decoder test on the swap here, unlike the Linux chain: both
        // decoders scale through `vpp_qsv`, so both need the pre-transpose
        // orientation.
        let swap_output = do_vpp_transpose && swap;
        let mut scale = hw_scale_filter(
            "vpp",
            "qsv",
            Some(out_format),
            swap_output,
            in_w,
            in_h,
            input.requested,
        );
        if !scale.is_empty() && is_d3d11va_decoder {
            // d3d11va has no dynamic pool: let the VPP filter allocate its own
            // frames rather than letting encoder look-ahead drain the
            // decoder's.
            scale.push_str(":passthrough=0");
        }
        if !scale.is_empty() && do_vpp_transpose {
            let _ = write!(scale, ":transpose={transpose_dir}");
        }
        if !scale.is_empty() && is_mjpeg_encoder {
            if vpp_full_range_out && !do_ocl_tonemap {
                scale.push_str(":out_range=pc");
            }
            if vpp_scale_mode_hq {
                scale.push_str(":scale_mode=hq");
            }
        }
        if !scale.is_empty() && do_vpp_tonemap {
            if procamp_params.is_empty() {
                if !two_pass_tonemap {
                    scale.push_str(":tonemap=1");
                }
            } else {
                scale.push_str(&procamp_params);
            }
        }

        if is_d3d11va_decoder && (!scale.is_empty() || input.deinterlace) {
            // The frame is a D3D11 surface; QSV is a view onto it.
            chain.main.push("hwmap=derive_device=qsv".to_owned());
        }
        if input.deinterlace {
            chain.main.push(hw_deinterlace_filter(
                caps,
                options,
                input.reference_frame_rate,
                "qsv",
            ));
        }
        chain.main.push(scale);

        if do_vpp_tonemap && two_pass_tonemap {
            chain
                .main
                .push("vpp_qsv=tonemap=1:format=nv12:async_depth=2".to_owned());
        }
        if do_vpp_tonemap {
            // Forced, not derived: upstream re-states bt709 in case the VPP
            // tonemap silently did not run — an MSDK runtime rather than VPL
            // will ignore the option instead of failing.
            chain.main.push(overwrite_color_properties_param(
                input.color_transfer,
                false,
            ));
        }
    }

    if do_ocl_tonemap && is_hw_decoder {
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
            is_mjpeg_encoder,
        ));
    }

    let mut memory_output = false;
    let is_upload_for_ocl_tonemap = is_sw_decoder && do_ocl_tonemap;
    // Simpler than the Linux chain's, which also accepts a VAAPI decode —
    // there is no VAAPI here to map from.
    let is_hwmap_usable = is_sw_encoder && do_ocl_tonemap;
    if (is_hw_decoder && is_sw_encoder) || is_upload_for_ocl_tonemap {
        memory_output = true;
        chain.main.push(
            if is_hwmap_usable {
                "hwmap=mode=read"
            } else {
                "hwdownload"
            }
            .to_owned(),
        );
        chain.main.push("format=nv12".to_owned());
    }

    if is_sw_decoder && is_qsv_encoder {
        memory_output = true;
    }

    if memory_output && let SubtitleOverlay::Text { plain, .. } = input.subtitle {
        chain.main.push(plain.to_owned());
    }

    if is_qsv_in_qsv_out && do_ocl_tonemap {
        // No `extra_hw_frames` here, unlike the Linux chain's reverse map.
        chain
            .main
            .push("hwmap=derive_device=qsv:mode=write:reverse=1".to_owned());
        chain.main.push("format=qsv".to_owned());
    }

    push_qsv_subtitle_filters(
        &mut chain,
        input,
        in_w,
        in_h,
        is_qsv_in_qsv_out,
        memory_output,
    );
    chain
}

/// The subtitle and overlay lists, which both QSV chains build identically.
fn push_qsv_subtitle_filters(
    chain: &mut FilterChain,
    input: &ChainInput<'_>,
    in_w: Option<i32>,
    in_h: Option<i32>,
    is_qsv_in_qsv_out: bool,
    memory_output: bool,
) {
    if is_qsv_in_qsv_out {
        if input.subtitle.is_some() {
            // `overlay_qsv` rescales, so the plane is generated smaller to move
            // less across the bus — as on iHD, and unlike the AMD chain.
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
            chain.sub.push(format!(
                "hwupload=derive_device=qsv:extra_hw_frames={QSV_SUBTITLE_POOL_FRAMES}"
            ));

            let (overlay_w, overlay_h) = fixed_output_size(in_w, in_h, input.requested);
            let overlay_size = match (overlay_w, overlay_h) {
                (Some(w), Some(h)) => format!(":w={w}:h={h}"),
                _ => String::new(),
            };
            chain.overlay.push(format!(
                "overlay_qsv=eof_action=pass:repeatlast=0{overlay_size}"
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
    }
}

/// The Linux QSV pipeline, layered on VAAPI. Port of
/// `GetIntelQsvVaapiVidFiltersPrefered`.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear pipeline in ffmpeg's own filter order; splitting it \
              would scatter the sequence across helpers that all re-thread the \
              same frame-location state"
)]
#[allow(
    clippy::similar_names,
    reason = "`is_sw_decoder` and `is_sw_encoder` are upstream's own names for \
              the two ends of the pipeline, and the whole chain reads as a \
              transliteration; renaming either would cost more than it saves"
)]
pub fn intel_qsv_vaapi_vid_filters_prefered(input: &ChainInput<'_>) -> FilterChain {
    let caps = input.caps;
    let options = input.options;

    let is_vaapi_decoder = contains(input.video_decoder, "vaapi");
    let is_qsv_decoder = contains(input.video_decoder, "qsv");
    let is_qsv_encoder = contains(input.video_encoder, "qsv");
    let is_hw_decoder = is_vaapi_decoder || is_qsv_decoder;
    let is_sw_decoder = input.is_sw_decoder();
    let is_sw_encoder = !is_qsv_encoder;
    let is_mjpeg_encoder = contains(input.video_encoder, "mjpeg");
    let is_qsv_in_qsv_out = is_hw_decoder && is_qsv_encoder;

    // Note: no decoder test here, unlike the iHD VAAPI chain — QSV can reach
    // the VPP tonemap from either decoder.
    let do_va_vpp_tonemap = input.vpp_tonemap_available;
    let do_ocl_tonemap = !do_va_vpp_tonemap && input.do_hw_tonemap;
    let do_tonemap = do_va_vpp_tonemap || do_ocl_tonemap;

    let has_graphical_subs = matches!(input.subtitle, SubtitleOverlay::Graphical { .. });

    let rotation = input.rotation.unwrap_or(0);
    let transpose_dir = if rotation == 0 {
        ""
    } else {
        video_transpose_direction(input.rotation)
    };
    let do_vpp_transpose = !transpose_dir.is_empty();
    let swap = rotation.abs() == 90 && (is_sw_decoder || (is_hw_decoder && do_vpp_transpose));
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

    if is_sw_decoder {
        if input.deinterlace {
            chain
                .main
                .push(sw_deinterlace_filter(options, input.reference_frame_rate));
        }
        // Three cases, not two: a graphical subtitle is composited in software
        // here, and `overlay` wants planar yuv420p rather than nv12.
        let out_format = if do_ocl_tonemap {
            "yuv420p10le"
        } else if has_graphical_subs {
            "yuv420p"
        } else {
            "nv12"
        };
        let mut scale = sw_scale_filter(
            input.video_encoder,
            in_w,
            in_h,
            input.three_d_format,
            input.requested,
        );
        if is_mjpeg_encoder && !do_ocl_tonemap {
            scale = if scale.is_empty() {
                "scale=out_range=pc".to_owned()
            } else {
                format!("{scale}:out_range=pc")
            };
        }
        chain.main.push(scale);
        chain.main.push(format!("format={out_format}"));
        if do_ocl_tonemap {
            chain.main.push("hwupload=derive_device=opencl".to_owned());
        }
    } else if is_hw_decoder {
        let hw_filter_suffix = if is_vaapi_decoder { "vaapi" } else { "qsv" };
        let vpp_full_range_out = is_mjpeg_encoder
            && caps
                .ffmpeg_version()
                .is_some_and(|v| v >= MIN_FFMPEG_QSV_VPP_OUT_RANGE_OPTION);
        let vpp_scale_mode_hq = is_mjpeg_encoder
            && caps
                .ffmpeg_version()
                .is_some_and(|v| v >= MIN_FFMPEG_QSV_VPP_SCALE_MODE_OPTION);

        if input.deinterlace {
            chain.main.push(hw_deinterlace_filter(
                caps,
                options,
                input.reference_frame_rate,
                hw_filter_suffix,
            ));
        }
        // VAAPI transposes with its own filter; QSV does it inside the scaler.
        if is_vaapi_decoder && do_vpp_transpose {
            chain
                .main
                .push(format!("transpose_vaapi=dir={transpose_dir}"));
        }

        // A QSV transpose happens *after* the scale within one filter, so the
        // scaler has to emit the post-transpose shape itself rather than being
        // handed pre-swapped dimensions.
        let out_format = if do_tonemap {
            if (is_qsv_decoder && do_vpp_transpose) || input.is_hevc_rext {
                "p010"
            } else {
                ""
            }
        } else {
            "nv12"
        };
        let swap_output = is_qsv_decoder && do_vpp_transpose && swap;
        let hw_scale_prefix = if is_qsv_decoder { "vpp" } else { "scale" };
        let mut scale = hw_scale_filter(
            hw_scale_prefix,
            hw_filter_suffix,
            Some(out_format),
            swap_output,
            in_w,
            in_h,
            input.requested,
        );
        if !scale.is_empty() && is_qsv_decoder && do_vpp_transpose {
            let _ = write!(scale, ":transpose={transpose_dir}");
        }
        if !scale.is_empty() && is_mjpeg_encoder {
            if !((is_qsv_decoder && !vpp_full_range_out) || do_ocl_tonemap) {
                scale.push_str(":out_range=pc");
            }
            // The two scalers spell their quality option differently, and the
            // QSV one only gained it in ffmpeg 6.
            if is_qsv_decoder {
                if vpp_scale_mode_hq {
                    scale.push_str(":scale_mode=hq");
                }
            } else {
                scale.push_str(":mode=hq");
            }
        }
        if !scale.is_empty() && is_vaapi_decoder {
            let _ = write!(scale, ":extra_hw_frames={VAAPI_VPP_EXTRA_HW_FRAMES}");
        }
        chain.main.push(scale);
    }

    if do_va_vpp_tonemap && is_hw_decoder {
        // The tonemap lives on the VAAPI side, so a QSV frame maps down and
        // back. Both are views of the same Intel surface, so neither costs a
        // copy.
        if is_qsv_decoder {
            chain.main.push("hwmap=derive_device=vaapi".to_owned());
            chain.main.push("format=vaapi".to_owned());
        }
        chain.main.push(hw_tonemap_filter(
            caps,
            options,
            "vaapi",
            Some("nv12"),
            is_mjpeg_encoder,
        ));
        if is_qsv_decoder {
            chain.main.push("hwmap=derive_device=qsv".to_owned());
            chain.main.push("format=qsv".to_owned());
        }
    }

    if do_ocl_tonemap && is_hw_decoder {
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
            is_mjpeg_encoder,
        ));
    }

    let mut memory_output = false;
    let is_upload_for_ocl_tonemap = is_sw_decoder && do_ocl_tonemap;
    // NOTE the polarity: upstream names this one `isHwmapUsable`, where the
    // VAAPI chains name theirs `isHwmapNotUsable`. Same decision, opposite
    // sense, and the QSV one is gated on the *encoder* being software because
    // QSV's own hwmap is only partly implemented.
    let is_hwmap_usable = is_sw_encoder && (do_ocl_tonemap || is_vaapi_decoder);
    if (is_hw_decoder && is_sw_encoder) || is_upload_for_ocl_tonemap {
        memory_output = true;
        chain.main.push(
            if is_hwmap_usable {
                "hwmap=mode=read"
            } else {
                "hwdownload"
            }
            .to_owned(),
        );
        chain.main.push("format=nv12".to_owned());
    }

    if is_sw_decoder && is_qsv_encoder {
        memory_output = true;
    }

    if memory_output && let SubtitleOverlay::Text { plain, .. } = input.subtitle {
        chain.main.push(plain.to_owned());
    }

    if is_qsv_in_qsv_out {
        if do_ocl_tonemap {
            chain.main.push(format!(
                "hwmap=derive_device=qsv:mode=write:reverse=1:\
                 extra_hw_frames={QSV_REVERSE_MAP_EXTRA_FRAMES}"
            ));
            chain.main.push("format=qsv".to_owned());
        } else if is_vaapi_decoder {
            chain.main.push("hwmap=derive_device=qsv".to_owned());
            chain.main.push("format=qsv".to_owned());
        }
    }

    // Byte-identical to the D3D11 chain's, which is why it is shared: both
    // composite with `overlay_qsv` onto a QSV surface.
    push_qsv_subtitle_filters(
        &mut chain,
        input,
        in_w,
        in_h,
        is_qsv_in_qsv_out,
        memory_output,
    );
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{FfmpegVersion, REQUIRED_FILTERS};
    use crate::encoding_helper::hw::capabilities::{FfmpegCapabilities, Platform};
    use crate::encoding_helper::hw::decoder::RequestedSize;
    use ferrofin_model::configuration::EncodingOptions;

    // Goldens derived from the C# (10.11.z 4324-4935) by a transliteration
    // written without reference to this file. Upstream ships no tests.

    const QSV_DECODER: &str =
        " -hwaccel qsv -hwaccel_output_format qsv -noautorotate -c:v h264_qsv";
    const VAAPI_DECODER: &str = " -hwaccel vaapi -hwaccel_output_format vaapi";
    const SDR_PARAMS: &str = "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";
    const HDR_PARAMS: &str =
        "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc";
    const OCL_TONEMAP: &str =
        "tonemap_opencl=format=nv12:p=bt709:t=bt709:m=bt709:tonemap=bt2390:peak=100:desat=0";
    const VAAPI_TONEMAP: &str = "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:p=bt709:t=bt709:\
                                 m=bt709:extra_hw_frames=32";

    fn caps_with(ffmpeg: FfmpegVersion) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["qsv", "vaapi", "opencl", "drm"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(ffmpeg)
            .build()
    }

    fn caps() -> FfmpegCapabilities {
        caps_with(FfmpegVersion::with_build(7, 0, 1))
    }

    fn options() -> EncodingOptions {
        EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::qsv,
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
            vulkan_tonemap_available: false,
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

    #[test]
    fn a_qsv_decode_scales_with_vpp_and_a_vaapi_decode_with_scale() {
        // Both decoders reach this one chain, and the filter vocabulary follows
        // whichever arrived — including `extra_hw_frames`, which is VAAPI-only.
        let caps = caps();
        let options = options();
        let mut qsv = input(&caps, &options, QSV_DECODER, "h264_qsv");
        qsv.requested = bounded(1280, 720);
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&qsv).main,
            vec![
                SDR_PARAMS.to_owned(),
                "vpp_qsv=w=1280:h=720:format=nv12".to_owned(),
            ]
        );

        let mut vaapi = input(&caps, &options, VAAPI_DECODER, "h264_qsv");
        vaapi.requested = bounded(1280, 720);
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&vaapi).main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale_vaapi=w=1280:h=720:format=nv12:extra_hw_frames=24".to_owned(),
                "hwmap=derive_device=qsv".to_owned(),
                "format=qsv".to_owned(),
            ]
        );
    }

    #[test]
    fn a_software_decode_is_handed_to_qsv_in_memory() {
        // No `hwupload_*` anywhere in this chain, unlike both VAAPI ones: a QSV
        // encoder takes system-memory frames directly.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_qsv");
        inp.requested = bounded(1280, 720);
        let chain = intel_qsv_vaapi_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                "scale=trunc(min(max(iw\\,ih*a)\\,min(1280\\,720*a))/2)*2:\
                 trunc(min(max(iw/a\\,ih)\\,min(1280/a\\,720))/2)*2"
                    .replace(' ', ""),
                "format=nv12".to_owned(),
            ]
        );
        assert!(
            !chain.main.iter().any(|f| f.contains("hwupload")),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn a_graphical_subtitle_forces_planar_output_from_a_software_decode() {
        // A three-way choice unique to this chain: `overlay` composites in
        // software here and wants planar yuv420p, not nv12.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_qsv");
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = intel_qsv_vaapi_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                String::new(),
                "format=yuv420p".to_owned()
            ]
        );
        assert_eq!(chain.sub, vec!["scale,scale=1920:1080:fast_bilinear"]);
        // ...and no upload tail on the overlay, unlike the VAAPI chains.
        assert_eq!(chain.overlay, vec!["overlay=eof_action=pass:repeatlast=0"]);
    }

    #[test]
    fn qsv_copies_back_where_vaapi_maps_because_its_hwmap_is_incomplete() {
        // The same job, differing only in which decoder produced the frame.
        // Upstream's comment: "qsv hwmap is not fully implemented for the time
        // being."
        let caps = caps();
        let options = options();
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&input(&caps, &options, QSV_DECODER, "libx264"))
                .main,
            vec![
                SDR_PARAMS.to_owned(),
                "vpp_qsv=format=nv12".to_owned(),
                "hwdownload".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&input(&caps, &options, VAAPI_DECODER, "libx264"))
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
    fn a_qsv_encoder_vetoes_the_map_even_when_tonemapping_would_allow_it() {
        // `isHwmapUsable` needs a SOFTWARE encoder as well as the tonemap, so
        // this pairing copies despite `doOclTonemap` being true.
        let caps = caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, "", "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "format=yuv420p10le".to_owned(),
                "hwupload=derive_device=opencl".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwdownload".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
    }

    #[test]
    fn qsv_borrows_vaapis_tonemap_and_comes_straight_back() {
        // QSV has no VPP tonemap of its own. Both maps are views of the same
        // Intel surface, so neither costs a copy.
        let caps = caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwmap=derive_device=vaapi".to_owned(),
                "format=vaapi".to_owned(),
                VAAPI_TONEMAP.replace(' ', ""),
                "hwmap=derive_device=qsv".to_owned(),
                "format=qsv".to_owned(),
            ]
        );

        // A VAAPI decode is already there, so it only maps outward — and that
        // map comes from the qsv-in-qsv-out block, not the tonemap block.
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                VAAPI_TONEMAP.replace(' ', ""),
                "hwmap=derive_device=qsv".to_owned(),
                "format=qsv".to_owned(),
            ]
        );
    }

    #[test]
    fn the_reverse_map_out_of_opencl_carries_its_own_pool_size() {
        // Without the extra frames `hevc_qsv` fails with "cannot allocate
        // memory" — the iHD VAAPI equivalent needs no such suffix.
        let caps = caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwmap=derive_device=opencl:mode=read".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwmap=derive_device=qsv:mode=write:reverse=1:extra_hw_frames=16".to_owned(),
                "format=qsv".to_owned(),
            ]
        );
    }

    #[test]
    fn a_qsv_scaler_states_its_size_before_its_own_transpose() {
        // `vpp_qsv` scales AND transposes in one filter, so its `w=`/`h=` are
        // the PRE-transpose orientation: 1080x1920 bounded by 1280x720 gives
        // 404x720, emitted as `w=720:h=404` with the rotation applied after.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.rotation = Some(90);
        inp.requested = bounded(1280, 720);
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "vpp_qsv=w=720:h=404:format=nv12:transpose=cclock".to_owned(),
            ]
        );

        // A VAAPI decode transposes with a separate preceding filter, so its
        // scaler sees the already-rotated frame and states the final size.
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_qsv");
        inp.rotation = Some(90);
        inp.requested = bounded(1280, 720);
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "transpose_vaapi=dir=cclock".to_owned(),
                "scale_vaapi=w=404:h=720:format=nv12:extra_hw_frames=24".to_owned(),
                "hwmap=derive_device=qsv".to_owned(),
                "format=qsv".to_owned(),
            ]
        );
    }

    #[test]
    fn qsv_composites_subtitles_on_the_gpu_with_a_fixed_pool() {
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.requested = bounded(1280, 720);
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = intel_qsv_vaapi_vid_filters_prefered(&inp);
        assert_eq!(
            chain.sub,
            vec![
                "alphasrc=s=1280x720:r=10:start='0'".to_owned(),
                "format=bgra".to_owned(),
                "subtitles=f='/media/a.srt':alpha=1:sub2video=1".to_owned(),
                // QSV needs a fixed pool; smaller values fail on some iGPUs.
                "hwupload=derive_device=qsv:extra_hw_frames=64".to_owned(),
            ]
        );
        assert_eq!(
            chain.overlay,
            vec!["overlay_qsv=eof_action=pass:repeatlast=0:w=1280:h=720"]
        );
    }

    #[test]
    fn a_vaapi_decode_into_qsv_still_counts_as_gpu_to_gpu() {
        // `isQsvInQsvOut` is `isHwDecoder && isQsvEncoder`, NOT a matching
        // pair — so a VAAPI decode takes the `overlay_qsv` path too.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, VAAPI_DECODER, "h264_qsv");
        inp.requested = bounded(1280, 720);
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = intel_qsv_vaapi_vid_filters_prefered(&inp);
        assert_eq!(
            chain.overlay,
            vec!["overlay_qsv=eof_action=pass:repeatlast=0:w=1280:h=720"]
        );
        assert!(chain.sub.contains(&"format=bgra".to_owned()));
    }

    #[test]
    fn the_two_scalers_spell_their_mjpeg_options_differently() {
        // `:scale_mode=hq` on vpp_qsv, `:mode=hq` on scale_vaapi — and only the
        // QSV spellings are version-gated.
        let options = options();
        let latest = caps();
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&input(
                &latest,
                &options,
                QSV_DECODER,
                "mjpeg_qsv"
            ))
            .main[1],
            "vpp_qsv=format=nv12:out_range=pc:scale_mode=hq"
        );
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&input(
                &latest,
                &options,
                VAAPI_DECODER,
                "mjpeg_qsv"
            ))
            .main[1],
            "scale_vaapi=format=nv12:out_range=pc:mode=hq:extra_hw_frames=24"
        );
    }

    #[test]
    fn the_two_qsv_option_gates_move_independently() {
        let options = options();
        // Below both: neither option.
        let old = caps_with(FfmpegVersion::new(5, 1));
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&input(&old, &options, QSV_DECODER, "mjpeg_qsv"))
                .main[1],
            "vpp_qsv=format=nv12"
        );
        // Between them: the scale mode arrived in 6.0, the range in 7.0.1.
        let mid = caps_with(FfmpegVersion::new(6, 0));
        assert_eq!(
            intel_qsv_vaapi_vid_filters_prefered(&input(&mid, &options, QSV_DECODER, "mjpeg_qsv"))
                .main[1],
            "vpp_qsv=format=nv12:scale_mode=hq"
        );
    }

    #[test]
    fn a_software_decode_loses_its_tonemap_entirely() {
        // An UPSTREAM BUG, reproduced deliberately. `doVaVppTonemap` here has
        // no decoder test (the iHD VAAPI chain's does), so on a software decode
        // it is true, which suppresses the OpenCL tonemap -- but the emission
        // is guarded on a hardware decoder and never fires. The encoder is
        // handed HDR-tagged, un-tonemapped content in an 8-bit format.
        //
        // A port that "fixed" this would diverge from every Jellyfin server.
        let caps = caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, "", "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        let chain = intel_qsv_vaapi_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "format=nv12".to_owned()
            ]
        );
        assert!(
            !chain.main.iter().any(|f| f.contains("tonemap")),
            "{:?}",
            chain.main
        );
    }

    // ----- Windows / D3D11 ---------------------------------------------------

    const D3D11_DECODER: &str =
        " -hwaccel d3d11va -hwaccel_output_format d3d11 -noautorotate -threads 2";

    fn win_caps() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(Platform::Windows)
            .hwaccels(["qsv", "d3d11va", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build()
    }

    #[test]
    fn a_d3d11_decode_is_relayed_into_qsv_with_its_own_frame_pool() {
        // d3d11va has no dynamic pool, so the VPP filter is made to allocate
        // its own frames rather than letting encoder look-ahead drain the
        // decoder's.
        let caps = win_caps();
        let options = options();
        let mut inp = input(&caps, &options, D3D11_DECODER, "h264_qsv");
        inp.requested = bounded(1280, 720);
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "hwmap=derive_device=qsv".to_owned(),
                "vpp_qsv=w=1280:h=720:format=nv12:passthrough=0".to_owned(),
            ]
        );
    }

    #[test]
    fn the_relay_follows_the_scaler_not_the_decoder() {
        // The relay is guarded on there being something to relay FOR. With an
        // OpenCL tonemap and no resize the scaler comes out empty, and the
        // d3d11 frame goes straight into the OpenCL interop.
        let caps = win_caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, D3D11_DECODER, "libx264");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        let chain = intel_qsv_dx11_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwmap=derive_device=opencl:mode=read".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwmap=mode=read".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
        assert!(
            !chain.main.iter().any(|f| f == "hwmap=derive_device=qsv"),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn the_deinterlacer_sits_between_the_relay_and_the_scaler() {
        // The reverse of every other chain, where the deinterlacer precedes the
        // scaler with nothing between. Here the scaler is BUILT early and
        // APPENDED late.
        let caps = win_caps();
        let options = options();
        let mut inp = input(&caps, &options, D3D11_DECODER, "h264_qsv");
        inp.deinterlace = true;
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main,
            vec![
                SDR_PARAMS.to_owned(),
                "hwmap=derive_device=qsv".to_owned(),
                // QSV's deinterlacer takes no rate argument at all.
                "deinterlace_qsv=mode=2".to_owned(),
                "vpp_qsv=format=nv12:passthrough=0".to_owned(),
            ]
        );
    }

    #[test]
    fn a_stock_windows_server_tonemaps_in_two_passes() {
        // `VppTonemappingBrightness` defaults to 16, which turns procamp on,
        // which forces the tonemap into its own second filter with the first
        // producing p010 for it. The two-pass form is the DEFAULT here; the
        // single-pass one is the unusual case.
        let caps = win_caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                "vpp_qsv=format=p010:brightness=16:procamp=1:async_depth=2".to_owned(),
                "vpp_qsv=tonemap=1:format=nv12:async_depth=2".to_owned(),
                // Re-stated, not derived: an MSDK runtime ignores the tonemap
                // option instead of failing, so upstream forces bt709 at the
                // tail regardless. The only chain that emits `setparams` twice.
                SDR_PARAMS.to_owned(),
            ]
        );
    }

    #[test]
    fn without_procamp_the_tonemap_fuses_into_the_scaler() {
        let caps = win_caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            vpp_tonemapping_brightness: 0.0,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                // `format=nv12`, not an empty format: the first ternary keys on
                // the OPENCL tonemap, so a VPP tonemap leaves it at nv12.
                "vpp_qsv=format=nv12:tonemap=1".to_owned(),
                SDR_PARAMS.to_owned(),
            ]
        );
    }

    #[test]
    fn range_extensions_split_the_passes_without_any_procamp_suffix() {
        // Two-pass by the other route. The first pass gets NO suffix at all —
        // a bare `vpp_qsv=format=p010`, not a missing element.
        let caps = win_caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            vpp_tonemapping_brightness: 0.0,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        inp.is_hevc_rext = true;
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                "vpp_qsv=format=p010".to_owned(),
                "vpp_qsv=tonemap=1:format=nv12:async_depth=2".to_owned(),
                SDR_PARAMS.to_owned(),
            ]
        );
    }

    #[test]
    fn contrast_alone_also_turns_procamp_on() {
        let caps = win_caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            enable_vpp_tonemapping: true,
            vpp_tonemapping_brightness: 0.0,
            vpp_tonemapping_contrast: 3.0,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        inp.vpp_tonemap_available = true;
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main[1],
            "vpp_qsv=format=p010:contrast=3:procamp=1:async_depth=2"
        );
    }

    #[test]
    fn the_windows_reverse_map_carries_no_pool_size() {
        // Unlike the Linux chain's, which needs `:extra_hw_frames=16`.
        let caps = win_caps();
        let options = EncodingOptions {
            enable_tonemapping: true,
            ..options()
        };
        let mut inp = input(&caps, &options, QSV_DECODER, "h264_qsv");
        inp.color_transfer = Some("smpte2084");
        inp.do_hw_tonemap = true;
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&inp).main,
            vec![
                HDR_PARAMS.to_owned(),
                String::new(),
                "hwmap=derive_device=opencl:mode=read".to_owned(),
                OCL_TONEMAP.to_owned(),
                "hwmap=derive_device=qsv:mode=write:reverse=1".to_owned(),
                "format=qsv".to_owned(),
            ]
        );
    }

    #[test]
    fn a_d3d11_decode_into_a_software_encoder_always_copies() {
        // `isHwmapUsable` here is just `isSwEncoder && doOclTonemap` — there is
        // no VAAPI decoder to map from, so the Linux chain's extra disjunct is
        // simply absent.
        let caps = win_caps();
        let options = options();
        assert_eq!(
            intel_qsv_dx11_vid_filters_prefered(&input(&caps, &options, D3D11_DECODER, "libx264"))
                .main,
            vec![
                SDR_PARAMS.to_owned(),
                "hwmap=derive_device=qsv".to_owned(),
                "vpp_qsv=format=nv12:passthrough=0".to_owned(),
                "hwdownload".to_owned(),
                "format=nv12".to_owned(),
            ]
        );
    }

    #[test]
    fn both_windows_decoders_state_the_pre_transpose_size() {
        // Unlike Linux, where only a QSV decode does: this chain always emits
        // `vpp_qsv` and always transposes inside it, so both decoders need the
        // swap. A d3d11va decode gets `w=720:h=404` where its Linux VAAPI
        // counterpart would get `w=404:h=720` plus a separate transpose filter.
        let caps = win_caps();
        let options = options();
        for decoder in [QSV_DECODER, D3D11_DECODER] {
            let mut inp = input(&caps, &options, decoder, "h264_qsv");
            inp.rotation = Some(90);
            inp.requested = bounded(1280, 720);
            let main = intel_qsv_dx11_vid_filters_prefered(&inp).main;
            assert!(
                main.last().unwrap().contains("w=720:h=404"),
                "{decoder}: {main:?}"
            );
            assert!(
                main.last().unwrap().ends_with(":transpose=cclock"),
                "{decoder}: {main:?}"
            );
        }
        assert!(
            !intel_qsv_dx11_vid_filters_prefered(&input(&caps, &options, QSV_DECODER, "h264_qsv"))
                .main
                .iter()
                .any(|f| f.contains("transpose_vaapi"))
        );
    }

    #[test]
    fn the_windows_subtitle_block_matches_the_linux_one() {
        // Byte-identical between the two chains, which is why they share it.
        let win = win_caps();
        let lin = caps();
        let options = options();
        let build = |caps: &FfmpegCapabilities, dx11: bool| {
            let mut inp = input(caps, &options, QSV_DECODER, "h264_qsv");
            inp.requested = bounded(1280, 720);
            inp.subtitle = SubtitleOverlay::Text {
                plain: "subtitles=f='/media/a.srt'",
                alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
                is_ass: false,
            };
            if dx11 {
                intel_qsv_dx11_vid_filters_prefered(&inp)
            } else {
                intel_qsv_vaapi_vid_filters_prefered(&inp)
            }
        };
        let a = build(&win, true);
        let b = build(&lin, false);
        assert_eq!(a.sub, b.sub);
        assert_eq!(a.overlay, b.overlay);
        assert_eq!(
            a.sub.last().unwrap(),
            "hwupload=derive_device=qsv:extra_hw_frames=64"
        );
    }

    // ----- the gate ----------------------------------------------------------

    #[test]
    fn a_job_touching_no_gpu_takes_the_software_chain() {
        let caps = caps();
        let options = options();
        assert_eq!(
            intel_vid_filter_chain(&input(&caps, &options, "", "libx264")).main,
            vec![
                SDR_PARAMS.to_owned(),
                String::new(),
                "format=yuv420p".to_owned()
            ]
        );
    }

    #[test]
    fn the_copy_back_branch_appends_nothing_at_all() {
        // Unlike the VAAPI gate, which appends `hwupload=derive_device=vaapi`
        // so its encoder gets VRAM frames. A QSV encoder does not need that.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["qsv", "vaapi", "opencl", "drm"])
            .filters(REQUIRED_FILTERS.into_iter().filter(|f| *f != "alphasrc"))
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options();
        let chain = intel_vid_filter_chain(&input(&without, &options, QSV_DECODER, "h264_qsv"));
        assert_eq!(
            chain.main,
            vec![
                SDR_PARAMS.to_owned(),
                String::new(),
                "format=nv12".to_owned()
            ]
        );
        assert!(
            !chain.main.iter().any(|f| f.contains("hwupload")),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn a_qsv_job_reaches_the_linux_chain_through_the_gate() {
        let caps = caps();
        let options = options();
        // `vpp_qsv` is the marker no other chain emits.
        assert!(
            intel_vid_filter_chain(&input(&caps, &options, QSV_DECODER, "h264_qsv"))
                .main
                .iter()
                .any(|f| f.starts_with("vpp_qsv"))
        );
    }

    #[test]
    fn a_different_accelerator_yields_nothing_at_all() {
        let caps = caps();
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            ..EncodingOptions::default()
        };
        assert_eq!(
            intel_vid_filter_chain(&input(&caps, &options, VAAPI_DECODER, "h264_vaapi")),
            FilterChain::default()
        );
    }
}
