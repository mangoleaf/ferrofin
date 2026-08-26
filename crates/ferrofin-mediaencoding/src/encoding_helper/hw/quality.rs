//! The two hardware encoder-quality flags Intel needs.
//!
//! Port of the preamble of C# `GetVideoQualityParam` (10.11.z 2029-2105) — the
//! part before `GetEncoderParam`, which is entirely Intel-gated.
//!
//! It lives here rather than in [`super::super::helper`] because deciding it
//! needs the real [`FfmpegCapabilities`] (which VAAPI driver, which kernel),
//! and `EncodingHelper` is generic over the capability seam so a test fake can
//! stand in. The result is a prefix string the caller emits immediately before
//! the quality parameters, which puts the arguments in upstream's order.

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::HardwareAccelerationType;

use super::capabilities::FfmpegCapabilities;
use super::contains;
use super::decoder::{DecodeContext, hardware_video_decoder};
use super::input_args::is_hw_tonemap_available;
use super::support::{is_opencl_full_supported, is_vaapi_supported};
use super::tonemap::is_intel_vpp_tonemap_available;
use super::versions::{MAX_KERNEL_I915_HANG, MIN_FIXED_KERNEL_60_I915_HANG, MIN_KERNEL_I915_HANG};

/// Whether Intel's low-power encoding entrypoint should be asked for.
///
/// Low power is a different encoder block on Intel silicon (VDEnc), not a
/// setting on the same one: it avoids CPU↔GPU synchronisation, which is what
/// makes 4K transcoding and tonemapping viable on an iGPU. It only exists on
/// Intel, which is why the VAAPI arm additionally tests the driver — a VAAPI
/// device that is AMD would be asked for an entrypoint it does not have.
#[must_use]
pub fn intel_low_power_encoding(
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    video_encoder: &str,
) -> bool {
    match options.hardware_acceleration_type {
        HardwareAccelerationType::vaapi => {
            let intel_driver =
                caps.is_vaapi_device_intel_ihd() || caps.is_vaapi_device_intel_i965();
            if video_encoder.eq_ignore_ascii_case("h264_vaapi") {
                options.enable_intel_low_power_h264_hw_encoder && intel_driver
            } else if video_encoder.eq_ignore_ascii_case("hevc_vaapi") {
                options.enable_intel_low_power_hevc_hw_encoder && intel_driver
            } else {
                false
            }
        }
        // QSV is Intel by definition, so no driver test here.
        HardwareAccelerationType::qsv => {
            if video_encoder.eq_ignore_ascii_case("h264_qsv") {
                options.enable_intel_low_power_h264_hw_encoder
            } else if video_encoder.eq_ignore_ascii_case("hevc_qsv") {
                options.enable_intel_low_power_hevc_hw_encoder
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Whether the i915 hang workaround applies. Port of `enableWaFori915Hang`.
///
/// Linux 5.18 through 6.1.3 can hang the Intel GPU when a QSV/VAAPI decode
/// feeds an OpenCL tonemap; `-async_depth 1` serialises the pipeline enough to
/// avoid it, at a real cost in throughput. 6.0.18 and later 6.0.x carry the
/// backported fix, which is why that one series is excused inside the range.
///
/// It is QSV-only: upstream computes it in the QSV arm and explicitly clears it
/// for any QSV encoder that is not h264/hevc.
#[must_use]
pub fn i915_hang_workaround(
    ctx: &DecodeContext<'_>,
    options: &EncodingOptions,
    video_encoder: &str,
) -> bool {
    if options.hardware_acceleration_type != HardwareAccelerationType::qsv {
        return false;
    }
    // Upstream's trailing `else { enableWaFori915Hang = false; }`: the
    // workaround is only kept for the two encoders it was measured on.
    if !video_encoder.eq_ignore_ascii_case("h264_qsv")
        && !video_encoder.eq_ignore_ascii_case("hevc_qsv")
    {
        return false;
    }
    let caps = ctx.caps;
    if !caps.platform().is_linux() {
        return false;
    }
    let Some(kernel) = caps.os_version() else {
        // An unreadable kernel version fails the range test in C# too: its
        // unparseable `0.0.0.0` is below the affected range.
        return false;
    };
    let fixed_60 =
        kernel.major() == 6 && kernel.minor() == 0 && kernel >= MIN_FIXED_KERNEL_60_I915_HANG;
    let unaffected = kernel < MIN_KERNEL_I915_HANG || kernel > MAX_KERNEL_I915_HANG;
    if unaffected || fixed_60 {
        return false;
    }

    let video_decoder = hardware_video_decoder(ctx).unwrap_or_default();
    let intel_decoder = contains(&video_decoder, "qsv") || contains(&video_decoder, "vaapi");
    // The hang needs the OpenCL tonemap specifically — the VPP one does not
    // trigger it, which is why its availability *suppresses* the workaround.
    let ocl_tonemap = caps.supports_hwaccel("qsv")
        && is_vaapi_supported(caps, ctx.video_stream.and_then(|s| s.codec.as_deref()))
        && is_opencl_full_supported(caps)
        && !is_intel_vpp_tonemap_available(caps, options, ctx.video_stream)
        && is_hw_tonemap_available(ctx, &video_decoder);
    intel_decoder && ocl_tonemap
}

/// The two flags as the argument fragment upstream prepends to the quality
/// parameters, in its order. Empty when neither applies.
#[must_use]
pub fn hardware_quality_preamble(
    ctx: &DecodeContext<'_>,
    options: &EncodingOptions,
    video_encoder: &str,
) -> String {
    let mut param = String::new();
    if intel_low_power_encoding(ctx.caps, options, video_encoder) {
        param.push_str(" -low_power 1");
    }
    if i915_hang_workaround(ctx, options, video_encoder) {
        param.push_str(" -async_depth 1");
    }
    param
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{FfmpegVersion, REQUIRED_FILTERS};
    use crate::encoding_helper::hw::capabilities::Platform;
    use crate::encoding_helper::hw::decoder::RequestedSize;
    use ferrofin_model::data::{VideoRange, VideoRangeType};
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_model::entities_media::MediaStream;
    use rstest::rstest;

    fn caps(kernel: Option<FfmpegVersion>, ihd: bool) -> FfmpegCapabilities {
        let mut b = FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi", "qsv", "opencl", "drm"])
            .filters(REQUIRED_FILTERS)
            .all_filter_options(true)
            .vaapi_driver(false, ihd, false)
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1));
        if let Some(k) = kernel {
            b = b.os_version(k);
        }
        b.build()
    }

    fn hdr_stream() -> MediaStream {
        MediaStream {
            codec: Some("hevc".to_owned()),
            index: 0,
            stream_type: MediaStreamType::Video,
            pixel_format: Some("yuv420p10le".to_owned()),
            bit_depth: Some(10),
            video_range: Some(VideoRange::Hdr),
            // The VPP tonemap tests the RANGE TYPE specifically, where the
            // OpenCL one is satisfied by the range alone — so a fixture with
            // only the latter silently exercises the OpenCL path.
            video_range_type: Some(VideoRangeType::Hdr10),
            color_transfer: Some("smpte2084".to_owned()),
            width: Some(3840),
            height: Some(2160),
            ..MediaStream::default()
        }
    }

    fn ctx<'a>(
        caps: &'a FfmpegCapabilities,
        options: &'a EncodingOptions,
        stream: &'a MediaStream,
    ) -> DecodeContext<'a> {
        DecodeContext {
            caps,
            options,
            video_stream: Some(stream),
            video_type: None,
            output_video_codec: Some("h264"),
            requested: RequestedSize::default(),
        }
    }

    #[rstest]
    // Low power is an Intel encoder block, so a VAAPI device that is not Intel
    // must not be asked for it.
    #[case(HardwareAccelerationType::vaapi, "h264_vaapi", true, true)]
    #[case(HardwareAccelerationType::vaapi, "hevc_vaapi", true, true)]
    #[case(HardwareAccelerationType::vaapi, "h264_vaapi", false, false)]
    #[case(HardwareAccelerationType::vaapi, "av1_vaapi", true, false)]
    // QSV is Intel by definition, so the driver is not consulted at all.
    #[case(HardwareAccelerationType::qsv, "h264_qsv", false, true)]
    #[case(HardwareAccelerationType::qsv, "hevc_qsv", false, true)]
    #[case(HardwareAccelerationType::qsv, "av1_qsv", false, false)]
    #[case(HardwareAccelerationType::nvenc, "h264_nvenc", true, false)]
    fn low_power_is_asked_for_only_where_the_entrypoint_exists(
        #[case] accel: HardwareAccelerationType,
        #[case] encoder: &str,
        #[case] intel_driver: bool,
        #[case] expected: bool,
    ) {
        let caps = caps(None, intel_driver);
        let options = EncodingOptions {
            hardware_acceleration_type: accel,
            enable_intel_low_power_h264_hw_encoder: true,
            enable_intel_low_power_hevc_hw_encoder: true,
            ..EncodingOptions::default()
        };
        assert_eq!(
            intel_low_power_encoding(&caps, &options, encoder),
            expected,
            "{accel:?}/{encoder}/intel={intel_driver}"
        );
    }

    #[test]
    fn low_power_follows_its_own_per_codec_switch() {
        let caps = caps(None, true);
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            enable_intel_low_power_h264_hw_encoder: true,
            enable_intel_low_power_hevc_hw_encoder: false,
            ..EncodingOptions::default()
        };
        assert!(intel_low_power_encoding(&caps, &options, "h264_vaapi"));
        assert!(!intel_low_power_encoding(&caps, &options, "hevc_vaapi"));
    }

    #[rstest]
    // The affected range is 5.18 through 6.1.3 inclusive...
    #[case(FfmpegVersion::new(5, 17), false)]
    #[case(FfmpegVersion::new(5, 18), true)]
    #[case(FfmpegVersion::with_build(6, 1, 3), true)]
    #[case(FfmpegVersion::with_build(6, 1, 4), false)]
    // ...except that 6.0.18 and later 6.0.x carry the backported fix, which is
    // a hole INSIDE the range rather than a shortening of it.
    #[case(FfmpegVersion::with_build(6, 0, 17), true)]
    #[case(FfmpegVersion::with_build(6, 0, 18), false)]
    #[case(FfmpegVersion::with_build(6, 0, 19), false)]
    fn the_i915_workaround_covers_the_affected_kernels_only(
        #[case] kernel: FfmpegVersion,
        #[case] expected: bool,
    ) {
        let caps = caps(Some(kernel), true);
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::qsv,
            enable_tonemapping: true,
            enable_vpp_tonemapping: false,
            hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
            ..EncodingOptions::default()
        };
        let stream = hdr_stream();
        assert_eq!(
            i915_hang_workaround(&ctx(&caps, &options, &stream), &options, "h264_qsv"),
            expected,
            "kernel {kernel:?}"
        );
    }

    #[test]
    fn the_i915_workaround_needs_the_opencl_tonemap_specifically() {
        // The VPP tonemap does not trigger the hang, so its availability
        // suppresses the workaround rather than being irrelevant to it.
        let caps = caps(Some(FfmpegVersion::new(6, 0)), true);
        let stream = hdr_stream();
        let ocl = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::qsv,
            enable_tonemapping: true,
            hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
            ..EncodingOptions::default()
        };
        assert!(i915_hang_workaround(
            &ctx(&caps, &ocl, &stream),
            &ocl,
            "h264_qsv"
        ));

        let vpp = EncodingOptions {
            enable_vpp_tonemapping: true,
            ..ocl.clone()
        };
        assert!(!i915_hang_workaround(
            &ctx(&caps, &vpp, &stream),
            &vpp,
            "h264_qsv"
        ));

        // ...and with no tonemap at all there is nothing to work around.
        let none = EncodingOptions {
            enable_tonemapping: false,
            ..ocl.clone()
        };
        assert!(!i915_hang_workaround(
            &ctx(&caps, &none, &stream),
            &none,
            "h264_qsv"
        ));
    }

    #[test]
    fn the_i915_workaround_is_kept_only_for_the_encoders_it_was_measured_on() {
        let caps = caps(Some(FfmpegVersion::new(6, 0)), true);
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::qsv,
            enable_tonemapping: true,
            hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
            ..EncodingOptions::default()
        };
        let stream = hdr_stream();
        assert!(i915_hang_workaround(
            &ctx(&caps, &options, &stream),
            &options,
            "hevc_qsv"
        ));
        assert!(!i915_hang_workaround(
            &ctx(&caps, &options, &stream),
            &options,
            "av1_qsv"
        ));
    }

    #[test]
    fn the_i915_workaround_is_qsv_only() {
        // VAAPI runs the same OpenCL tonemap on the same silicon, but upstream
        // computes the workaround inside its QSV arm alone.
        let caps = caps(Some(FfmpegVersion::new(6, 0)), true);
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            enable_tonemapping: true,
            hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
            ..EncodingOptions::default()
        };
        let stream = hdr_stream();
        assert!(!i915_hang_workaround(
            &ctx(&caps, &options, &stream),
            &options,
            "h264_vaapi"
        ));
    }

    #[test]
    fn the_preamble_emits_both_flags_in_upstreams_order() {
        let caps = caps(Some(FfmpegVersion::new(6, 0)), true);
        let options = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::qsv,
            enable_intel_low_power_h264_hw_encoder: true,
            enable_tonemapping: true,
            hardware_decoding_codecs: vec!["h264".to_owned(), "hevc".to_owned()],
            ..EncodingOptions::default()
        };
        let stream = hdr_stream();
        assert_eq!(
            hardware_quality_preamble(&ctx(&caps, &options, &stream), &options, "h264_qsv"),
            " -low_power 1 -async_depth 1"
        );

        // Neither applying leaves nothing at all, not a stray space.
        let plain = EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::nvenc,
            ..EncodingOptions::default()
        };
        assert_eq!(
            hardware_quality_preamble(&ctx(&caps, &plain, &stream), &plain, "h264_nvenc"),
            ""
        );
    }
}
