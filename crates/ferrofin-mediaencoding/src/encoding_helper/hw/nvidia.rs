//! The NVENC / CUDA filter chain.
//!
//! Port of C# `EncodingHelper.GetNvidiaVidFilterChain` and
//! `GetNvidiaVidFiltersPrefered` (10.11.z 3886–4084).
//!
//! The chain's shape is decided by **where the frames are** at each step, and
//! there are four combinations of that, not one pipeline:
//!
//! | decode | encode | frames live in |
//! |---|---|---|
//! | CUDA | NVENC | VRAM throughout — nothing is copied |
//! | CUDA | software | VRAM, then downloaded once for the encoder |
//! | software | NVENC | system memory, uploaded only if tonemapping needs it |
//! | software | software | this chain does not apply at all |
//!
//! Every filter below is placed by that table. A `hwupload` appears only where
//! a CPU-side frame has to reach a GPU filter; a `hwdownload` only where a
//! GPU-side frame has to reach a CPU-side encoder or the `subtitles` filter,
//! which has no CUDA equivalent.

use ferrofin_model::entities::HardwareAccelerationType;

use super::capabilities::FilterOption;
use super::contains;
use super::filters::{
    alpha_src_filter, graphical_sub_preprocess_filters, hw_deinterlace_filter, hw_scale_filter,
    sw_deinterlace_filter, video_transpose_direction,
};
use super::sw_chain::{ChainInput, FilterChain, SubtitleOverlay, sw_vid_filter_chain};
use super::tonemap::{hw_tonemap_filter, overwrite_color_properties_param};

/// The frame rate a non-ASS text subtitle is generated at.
///
/// Port of the literal `10` in `GetNvidiaVidFiltersPrefered`. Plain subtitles
/// change only when a caption appears or disappears, so generating the
/// transparent overlay source at the video's rate would burn GPU time drawing
/// identical frames.
pub const STATIC_SUBTITLE_FRAMERATE: f32 = 10.0;

/// The ceiling on an ASS/SSA subtitle's generated frame rate.
///
/// Port of `Math.Min(framerate ?? 25, 60)`. ASS carries its own animation, so
/// it has to track the video — but not past 60fps, where the overlay would cost
/// more than the picture.
pub const MAX_ASS_SUBTITLE_FRAMERATE: f32 = 60.0;

/// The NVENC filter chain, falling back to software when the pipeline cannot
/// run. Port of `GetNvidiaVidFilterChain`.
///
/// The fallback is not a failure path: a job that neither decodes nor encodes
/// on the GPU has nothing for CUDA filters to act on, and a build missing
/// `alphasrc` cannot render text subtitles for a hardware overlay at all.
#[must_use]
pub fn nvidia_vid_filter_chain(input: &ChainInput<'_>) -> FilterChain {
    if input.options.hardware_acceleration_type != HardwareAccelerationType::nvenc {
        return FilterChain::default();
    }

    let is_sw_decoder = input.is_sw_decoder();
    let is_sw_encoder = !contains(input.video_encoder, "nvenc");

    if (is_sw_decoder && is_sw_encoder)
        || !super::support::is_cuda_full_supported(input.caps)
        || !input.caps.supports_filter("alphasrc")
    {
        return sw_vid_filter_chain(input);
    }

    nvidia_vid_filters_prefered(input)
}

/// The CUDA pipeline proper. Port of `GetNvidiaVidFiltersPrefered`.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear pipeline in ffmpeg's own filter order; splitting it \
              would scatter the sequence across helpers that all re-thread the \
              same frame-location state"
)]
pub fn nvidia_vid_filters_prefered(input: &ChainInput<'_>) -> FilterChain {
    let caps = input.caps;
    let options = input.options;

    let is_nv_decoder = contains(input.video_decoder, "cuda");
    let is_nvenc_encoder = contains(input.video_encoder, "nvenc");
    let is_sw_decoder = input.is_sw_decoder();
    let is_sw_encoder = !is_nvenc_encoder;
    let is_mjpeg_encoder = contains(input.video_encoder, "mjpeg");
    // The zero-copy case: frames arrive in VRAM and leave from VRAM.
    let is_cu_in_cu_out = is_nv_decoder && is_nvenc_encoder;

    let do_cu_tonemap = input.do_hw_tonemap;

    let rotation = input.rotation.unwrap_or(0);
    let transpose_dir = if rotation == 0 {
        ""
    } else {
        video_transpose_direction(input.rotation)
    };
    let do_cu_transpose = !transpose_dir.is_empty() && caps.supports_filter("transpose_cuda");
    // The scaler only produces the pre-rotation shape when a transpose will
    // actually follow — on the CPU it always does, on the GPU only if the
    // filter exists.
    let swap = rotation.abs() == 90 && (is_sw_decoder || (is_nv_decoder && do_cu_transpose));
    let (in_w, in_h) = if swap {
        (input.video_height, input.video_width)
    } else {
        (input.video_width, input.video_height)
    };

    let mut chain = FilterChain::default();
    chain.main.push(overwrite_color_properties_param(
        input.color_transfer,
        do_cu_tonemap,
    ));

    if is_sw_decoder {
        // Frames are in system memory.
        if input.deinterlace {
            chain
                .main
                .push(sw_deinterlace_filter(options, input.reference_frame_rate));
        }
        // Tonemapping needs the extra bits, so the CPU-side format widens.
        let out_format = if do_cu_tonemap {
            "yuv420p10le"
        } else {
            "yuv420p"
        };
        chain.main.push(super::sw_chain::sw_scale_filter(
            input.video_encoder,
            in_w,
            in_h,
            input.three_d_format,
            input.requested,
        ));
        chain.main.push(format!("format={out_format}"));

        if do_cu_tonemap {
            // The only reason to move CPU frames onto the GPU here.
            chain.main.push("hwupload=derive_device=cuda".to_owned());
        }
    }

    if is_nv_decoder {
        // Frames are already in VRAM.
        if input.deinterlace {
            chain.main.push(hw_deinterlace_filter(
                caps,
                options,
                input.reference_frame_rate,
                "cuda",
            ));
        }
        if do_cu_transpose {
            chain
                .main
                .push(format!("transpose_cuda=dir={transpose_dir}"));
        }
        // HEVC Range Extensions decode to a 10-bit surface, so the scaler has
        // to be told p010 before the tonemapper reads it.
        let out_format = if do_cu_tonemap {
            if input.is_hevc_rext { "p010" } else { "" }
        } else {
            "yuv420p"
        };
        chain.main.push(hw_scale_filter(
            "scale",
            "cuda",
            Some(out_format),
            false,
            in_w,
            in_h,
            input.requested,
        ));
    }

    if do_cu_tonemap {
        chain.main.push(hw_tonemap_filter(
            caps,
            options,
            "cuda",
            Some("yuv420p"),
            is_mjpeg_encoder,
        ));
    }

    // Whether the frames end up back in system memory, which decides where the
    // subtitles filter can run — it has no CUDA equivalent.
    let is_upload_for_cu_tonemap = is_sw_decoder && do_cu_tonemap;
    let has_subs = input.subtitle.is_some();
    let mut memory_output = false;

    if (is_nv_decoder && is_sw_encoder) || (is_upload_for_cu_tonemap && has_subs) {
        memory_output = true;
        chain.main.push("hwdownload".to_owned());
        chain.main.push("format=yuv420p".to_owned());
    }

    if is_sw_decoder && is_nvenc_encoder && !is_upload_for_cu_tonemap {
        memory_output = true;
    }

    if memory_output && let SubtitleOverlay::Text { plain, .. } = input.subtitle {
        chain.main.push(plain.to_owned());
    }

    if is_cu_in_cu_out {
        // Frames never leave VRAM, so the subtitle has to be uploaded and
        // composited by a CUDA filter.
        match input.subtitle {
            SubtitleOverlay::Graphical { width, height } => {
                chain.sub.push(graphical_sub_preprocess_filters(
                    in_w,
                    in_h,
                    width,
                    height,
                    input.requested,
                ));
                chain.sub.push("format=yuva420p".to_owned());
                chain.sub.push("hwupload=derive_device=cuda".to_owned());
                chain
                    .overlay
                    .push("overlay_cuda=eof_action=pass:repeatlast=0".to_owned());
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
                    input.requested,
                    Some(sub_framerate),
                    input.start_time_ticks,
                ));
                chain.sub.push("format=yuva420p".to_owned());
                chain.sub.push(alpha_sub2video.to_owned());
                chain.sub.push("hwupload=derive_device=cuda".to_owned());

                // Premultiplied alpha composites correctly; without the option
                // the overlay is emitted plain and edges fringe.
                let alpha_format =
                    if caps.supports_filter_with_option(FilterOption::OverlayCudaAlphaFormat) {
                        ":alpha_format=premultiplied"
                    } else {
                        ""
                    };
                chain.overlay.push(format!(
                    "overlay_cuda=eof_action=pass:repeatlast=0{alpha_format}"
                ));
            }
            SubtitleOverlay::None => {}
        }
    } else if let SubtitleOverlay::Graphical { width, height } = input.subtitle {
        // Frames are in system memory by now, so the plain CPU overlay works.
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

    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{FfmpegVersion, REQUIRED_FILTERS};
    use crate::encoding_helper::hw::capabilities::{FfmpegCapabilities, Platform};
    use crate::encoding_helper::hw::decoder::RequestedSize;
    use ferrofin_model::configuration::EncodingOptions;
    use ferrofin_model::entities::Video3DFormat;
    use rstest::rstest;

    // Hand-derived from the C# (10.11.z 3886-4084). Upstream ships no tests.

    fn caps() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda", "opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build()
    }

    fn options() -> EncodingOptions {
        EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
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

    #[test]
    fn a_job_touching_no_gpu_falls_back_to_the_software_chain() {
        // Software decode AND software encode: CUDA filters would have nothing
        // to act on.
        let caps = caps();
        let options = options();
        let chain = nvidia_vid_filter_chain(&input(&caps, &options, "", "libx264"));
        assert!(chain.main.iter().any(|f| f == "format=yuv420p"));
        assert!(!chain.main.iter().any(|f| f.contains("cuda")));

        // `libx264` alone does not prove the fallback fired — the CUDA path
        // would emit `format=yuv420p` for it too. A VAAPI encoder separates
        // them: only the software chain knows it needs nv12.
        let chain = nvidia_vid_filter_chain(&input(&caps, &options, "", "h264_vaapi"));
        assert!(
            chain.main.iter().any(|f| f == "format=nv12"),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn a_build_without_alphasrc_cannot_run_the_cuda_pipeline() {
        // Without it a text subtitle has no source to render onto, so upstream
        // declines the whole chain rather than the subtitle alone.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda"])
            .filters(REQUIRED_FILTERS.into_iter().filter(|f| *f != "alphasrc"))
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options();
        let chain =
            nvidia_vid_filter_chain(&input(&without, &options, " -hwaccel cuda", "h264_nvenc"));
        assert!(
            !chain.main.iter().any(|f| f.contains("cuda")),
            "{:?}",
            chain.main
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
            nvidia_vid_filter_chain(&input(&caps, &options, "", "h264_vaapi")),
            FilterChain::default()
        );
    }

    #[test]
    fn cuda_in_cuda_out_never_copies_the_frames() {
        // The zero-copy case: no hwupload, no hwdownload.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.requested = RequestedSize {
            width: Some(1280),
            height: Some(720),
            ..RequestedSize::default()
        };
        let chain = nvidia_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_owned(),
                "scale_cuda=w=1280:h=720:format=yuv420p".to_owned(),
            ]
        );
        assert!(!chain.main.iter().any(|f| f.contains("hwdownload")));
        assert!(!chain.main.iter().any(|f| f.contains("hwupload")));
    }

    #[test]
    fn a_cuda_decode_into_a_software_encoder_downloads_once() {
        let caps = caps();
        let options = options();
        let chain =
            nvidia_vid_filters_prefered(&input(&caps, &options, " -hwaccel cuda", "libx264"));
        let joined = chain.main.join(",");
        assert!(joined.contains("hwdownload,format=yuv420p"), "{joined}");
    }

    #[test]
    fn a_software_decode_uploads_only_when_tonemapping_needs_the_gpu() {
        let caps = caps();
        let options = options();
        // No tonemap: the frames stay on the CPU and NVENC takes them directly.
        let chain = nvidia_vid_filters_prefered(&input(&caps, &options, "", "h264_nvenc"));
        assert!(
            !chain.main.iter().any(|f| f.contains("hwupload")),
            "{:?}",
            chain.main
        );

        // With tonemapping the CUDA filter needs them in VRAM.
        let mut inp = input(&caps, &options, "", "h264_nvenc");
        inp.do_hw_tonemap = true;
        let chain = nvidia_vid_filters_prefered(&inp);
        let joined = chain.main.join(",");
        assert!(
            joined.contains("format=yuv420p10le,hwupload=derive_device=cuda"),
            "{joined}"
        );
        // The whole filter, not just its name — every byte of this reaches the
        // command line. No `:tonemap_mode=` (the default mode is in neither
        // upstream mode list), no `:param=`, no `:range=` (range is `auto`).
        assert!(
            joined.contains(
                "tonemap_cuda=format=yuv420p:p=bt709:t=bt709:m=bt709:\
                 tonemap=bt2390:peak=100:desat=0"
                    .replace(' ', "")
                    .as_str()
            ),
            "{joined}"
        );
        // With no subtitle to draw there is nothing to come back down for.
        assert!(!joined.contains("hwdownload"), "{joined}");
        // ...and the colour properties describe the frames ENTERING the graph,
        // so a tonemapping job stamps the HDR source, not the SDR result — the
        // tonemapper is the thing that has to read them.
        assert_eq!(
            chain.main[0],
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc"
        );
        // HLG is the other HDR curve and gets its own transfer.
        inp.color_transfer = Some("arib-std-b67");
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).main[0],
            "setparams=color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc"
        );
    }

    #[test]
    fn hevc_range_extensions_are_scaled_to_p010_before_tonemapping() {
        // RExt decodes to a 10-bit surface; the scaler has to say so or the
        // tonemapper reads the wrong format.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.do_hw_tonemap = true;
        inp.is_hevc_rext = true;
        let chain = nvidia_vid_filters_prefered(&inp);
        assert!(
            chain.main.iter().any(|f| f.contains("format=p010")),
            "{:?}",
            chain.main
        );

        // Non-RExt tonemapping leaves the format to the tonemapper.
        inp.is_hevc_rext = false;
        let chain = nvidia_vid_filters_prefered(&inp);
        assert!(
            !chain.main.iter().any(|f| f.contains("p010")),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn rotation_transposes_on_the_gpu_when_the_filter_exists() {
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(90);
        let chain = nvidia_vid_filters_prefered(&inp);
        assert!(
            chain.main.iter().any(|f| f == "transpose_cuda=dir=cclock"),
            "{:?}",
            chain.main
        );

        // Without `transpose_cuda` the rotation is skipped entirely — and so is
        // the dimension swap that only exists to feed it.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda"])
            .filters(
                REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "transpose_cuda"),
            )
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let mut inp = input(&without, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(90);
        let chain = nvidia_vid_filters_prefered(&inp);
        assert!(
            !chain.main.iter().any(|f| f.contains("transpose")),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn a_graphical_subtitle_is_uploaded_and_composited_on_the_gpu() {
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = nvidia_vid_filters_prefered(&inp);
        assert_eq!(
            chain.sub,
            vec![
                "scale,scale=1920:1080:fast_bilinear".to_owned(),
                "format=yuva420p".to_owned(),
                "hwupload=derive_device=cuda".to_owned(),
            ]
        );
        // NOTE: no `:alpha_format=premultiplied` here. Upstream sets that
        // option only on the TEXT branch — a pre-processed bitmap subtitle is
        // already straight-alpha, so the option would be wrong for it.
        assert_eq!(
            chain.overlay,
            vec!["overlay_cuda=eof_action=pass:repeatlast=0"]
        );
    }

    #[test]
    fn a_text_subtitle_is_rendered_onto_a_generated_transparent_source() {
        // The `subtitles` filter has no CUDA equivalent, so on the zero-copy
        // path the text is drawn onto an `alphasrc` frame and uploaded.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = nvidia_vid_filters_prefered(&inp);
        assert_eq!(
            chain.sub,
            vec![
                // A static subtitle needs only 10fps.
                "alphasrc=s=1920x1080:r=10:start='0'".to_owned(),
                "format=yuva420p".to_owned(),
                "subtitles=f='/media/a.srt':alpha=1:sub2video=1".to_owned(),
                "hwupload=derive_device=cuda".to_owned(),
            ]
        );
        // ...and the plain spelling does NOT appear in the main chain.
        assert!(
            !chain.main.iter().any(|f| f.contains("subtitles=")),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn an_ass_subtitle_tracks_the_video_frame_rate_up_to_sixty() {
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.ass'",
            alpha_sub2video: "subtitles=f='/media/a.ass':alpha=1:sub2video=1",
            is_ass: true,
        };
        // ASS carries its own animation, so it follows the video.
        inp.real_frame_rate = Some(23.976);
        assert!(
            nvidia_vid_filters_prefered(&inp).sub[0].contains(":r=23.976:"),
            "{:?}",
            nvidia_vid_filters_prefered(&inp).sub
        );
        // ...but never past 60.
        inp.real_frame_rate = Some(120.0);
        assert!(nvidia_vid_filters_prefered(&inp).sub[0].contains(":r=60:"));
        // An unknown rate falls back to 25.
        inp.real_frame_rate = None;
        assert!(nvidia_vid_filters_prefered(&inp).sub[0].contains(":r=25:"));
    }

    #[test]
    fn a_text_subtitle_burns_in_on_the_cpu_when_the_frames_are_already_there() {
        // CUDA decode into a software encoder: the frames come back to memory,
        // so the ordinary `subtitles` filter can run in the main chain.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "libx264");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = nvidia_vid_filters_prefered(&inp);
        assert_eq!(chain.main.last().unwrap(), "subtitles=f='/media/a.srt'");
        assert!(chain.sub.is_empty());
        assert!(chain.overlay.is_empty());
    }

    #[test]
    fn a_software_upload_for_tonemapping_comes_back_down_for_subtitles() {
        // The one case that both uploads and downloads: CPU decode, tonemap on
        // the GPU, then back to memory because a subtitle has to be drawn.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_nvenc");
        inp.do_hw_tonemap = true;
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let joined = nvidia_vid_filters_prefered(&inp).main.join(",");
        assert!(joined.contains("hwupload=derive_device=cuda"), "{joined}");
        assert!(joined.contains("hwdownload,format=yuv420p"), "{joined}");
        assert!(joined.ends_with("subtitles=f='/media/a.srt'"), "{joined}");
    }

    #[test]
    fn the_premultiplied_alpha_option_is_used_only_when_probed() {
        let options = options();
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .filter_option(FilterOption::OverlayCudaAlphaFormat, false)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let mut inp = input(&without, &options, " -hwaccel cuda", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).overlay,
            vec!["overlay_cuda=eof_action=pass:repeatlast=0"]
        );
    }

    #[test]
    fn a_software_decode_into_nvenc_burns_subtitles_before_the_upload() {
        // Nothing is on the GPU yet, so the ordinary `subtitles` filter runs in
        // the main chain and NVENC uploads the finished frame itself.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "subtitles=f='/media/a.srt'",
            alpha_sub2video: "subtitles=f='/media/a.srt':alpha=1:sub2video=1",
            is_ass: false,
        };
        let chain = nvidia_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_owned(),
                // No resize was asked for, so the software scaler emits nothing
                // and the entry stays in place as an empty slot.
                String::new(),
                "format=yuv420p".to_owned(),
                "subtitles=f='/media/a.srt'".to_owned(),
            ]
        );
        assert!(chain.sub.is_empty());
    }

    #[test]
    fn a_rotated_frame_is_scaled_to_its_post_transpose_shape() {
        // The swap feeds the transpose that follows: the scaler — and the
        // subtitle source sized from it — must use the rotated dimensions.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(90);
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).sub[0],
            "alphasrc=s=1080x1920:r=10:start='0'"
        );

        // Without `transpose_cuda` no rotation happens, so no swap either.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["cuda"])
            .filters(
                REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "transpose_cuda"),
            )
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let mut inp = input(&without, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(90);
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).sub[0],
            "alphasrc=s=1920x1080:r=10:start='0'"
        );
    }

    #[test]
    fn a_build_without_cuda_cannot_run_the_cuda_pipeline() {
        // The pipeline needs more than an nvenc encoder — a build with no CUDA
        // hwaccel at all has no device for the filters to run on.
        let without = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["opencl"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        let options = options();
        let chain =
            nvidia_vid_filter_chain(&input(&without, &options, " -hwaccel cuda", "h264_nvenc"));
        assert!(
            !chain.main.iter().any(|f| f.contains("cuda")),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn an_mjpeg_encoder_tonemaps_to_full_range() {
        // MJPEG carries no range flag, so the tonemapper has to produce pc
        // range or every decoder guesses.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "mjpeg");
        inp.do_hw_tonemap = true;
        let joined = nvidia_vid_filters_prefered(&inp).main.join(",");
        assert!(joined.contains(":range=pc"), "{joined}");

        inp.video_encoder = "h264_nvenc";
        let joined = nvidia_vid_filters_prefered(&inp).main.join(",");
        assert!(!joined.contains(":range=pc"), "{joined}");
    }

    #[test]
    fn a_one_eighty_rotation_is_reversed_without_swapping_the_dimensions() {
        // `swapWAndH` is gated on |rotation| == 90 specifically. A 180° turn
        // still transposes, but the frame keeps its shape — so the subtitle
        // source must stay 1920x1080.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(180);
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        let chain = nvidia_vid_filters_prefered(&inp);
        assert_eq!(
            chain.main,
            vec![
                "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_owned(),
                "transpose_cuda=dir=reversal".to_owned(),
                "scale_cuda=format=yuv420p".to_owned(),
            ]
        );
        assert_eq!(chain.sub[0], "alphasrc=s=1920x1080:r=10:start='0'");
    }

    #[test]
    fn a_seek_offsets_the_generated_subtitle_source() {
        // Without this the overlay restarts at zero and every caption after a
        // seek is out of sync. The separators are escaped for ffmpeg.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        inp.start_time_ticks = 90_000_000;
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).sub[0],
            r"alphasrc=s=1920x1080:r=10:start='00\:00\:09\.000'"
        );
    }

    #[test]
    fn a_rotated_hardware_scale_sizes_from_the_swapped_shape() {
        // 1080x1920 bounded by 1280x720 scales by 720/1920, then truncates to
        // an even width — 404, not the 1280 an unswapped frame would give.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(90);
        inp.requested = RequestedSize {
            max_width: Some(1280),
            max_height: Some(720),
            ..RequestedSize::default()
        };
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).main,
            vec![
                "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_owned(),
                "transpose_cuda=dir=cclock".to_owned(),
                "scale_cuda=w=404:h=720:format=yuv420p".to_owned(),
            ]
        );
    }

    #[test]
    fn a_rotated_graphical_subtitle_is_padded_to_the_swapped_aspect() {
        // The swapped frame is 0.5625 DAR against a 1.7778 subtitle, which is
        // too far apart to simply rescale — so it pads and crops instead.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.rotation = Some(90);
        inp.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).sub[0],
            r"scale,scale=-1:1920:fast_bilinear,crop,pad=max(1080\,iw):max(1920\,ih):(ow-iw)/2:(oh-ih)/2:black@0,crop=1080:1920"
        );
    }

    #[test]
    fn a_generated_subtitle_source_matches_the_requested_output_size() {
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.subtitle = SubtitleOverlay::Text {
            plain: "s",
            alpha_sub2video: "s",
            is_ass: false,
        };
        inp.requested = RequestedSize {
            width: Some(1280),
            height: Some(720),
            ..RequestedSize::default()
        };
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).sub[0],
            "alphasrc=s=1280x720:r=10:start='0'"
        );
    }

    #[test]
    fn a_three_d_source_is_cropped_to_one_view_before_nvenc() {
        // The software scaler owns the 3D templates, so the format reaches it
        // through this chain unchanged.
        let caps = caps();
        let options = options();
        let mut inp = input(&caps, &options, "", "h264_nvenc");
        inp.three_d_format = Some(Video3DFormat::HalfSideBySide);
        inp.requested = RequestedSize {
            width: Some(1280),
            height: Some(720),
            ..RequestedSize::default()
        };
        // The requested HEIGHT is unused: the 3D templates substitute width
        // only and derive the height from the aspect.
        assert_eq!(
            nvidia_vid_filters_prefered(&inp).main[1],
            r"crop=iw/2:ih:0:0,scale=(iw*2):ih,setdar=dar=a,\
              crop=min(iw\,ih*dar):min(ih\,iw/dar):(iw-min(iw\,iw*sar))/2:\
              (ih - min (ih\,ih/sar))/2,setsar=sar=1,scale=1280:trunc(1280/dar/2)*2"
                .replace("\\\n              ", "")
        );
    }

    #[rstest]
    // Doubling the rate is only worth it below 30fps — above that the output
    // asks for more frames than a client expects.
    #[case(" -hwaccel cuda", Some(25.0), "yadif_cuda=1:-1:0")]
    #[case(" -hwaccel cuda", Some(60.0), "yadif_cuda=0:-1:0")]
    #[case("", Some(25.0), "yadif=1:-1:0")]
    #[case("", Some(60.0), "yadif=0:-1:0")]
    // An unknown rate reaches the same answer by two different routes: the
    // hardware helper defaults to 60, the software one compares the nullable
    // and gets false.
    #[case(" -hwaccel cuda", None, "yadif_cuda=0:-1:0")]
    #[case("", None, "yadif=0:-1:0")]
    fn double_rate_deinterlacing_follows_the_source_frame_rate(
        #[case] decoder: &str,
        #[case] rate: Option<f32>,
        #[case] expected: &str,
    ) {
        let caps = caps();
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
            deinterlace_double_rate: true,
            ..EncodingOptions::default()
        };
        let mut inp = input(&caps, &options, decoder, "h264_nvenc");
        inp.deinterlace = true;
        inp.reference_frame_rate = rate;
        assert_eq!(nvidia_vid_filters_prefered(&inp).main[1], expected);
    }

    #[test]
    fn deinterlacing_runs_where_the_frames_are() {
        let caps = caps();
        let options = options();
        // On the GPU for a CUDA decode...
        let mut inp = input(&caps, &options, " -hwaccel cuda", "h264_nvenc");
        inp.deinterlace = true;
        assert!(
            nvidia_vid_filters_prefered(&inp)
                .main
                .iter()
                .any(|f| f == "yadif_cuda=0:-1:0")
        );

        // ...and on the CPU for a software decode.
        let mut inp = input(&caps, &options, "", "h264_nvenc");
        inp.deinterlace = true;
        assert!(
            nvidia_vid_filters_prefered(&inp)
                .main
                .iter()
                .any(|f| f == "yadif=0:-1:0")
        );
    }
}
