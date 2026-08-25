//! Turning HDR into something an SDR screen can show.
//!
//! Port of C# `EncodingHelper`'s tonemapping block: four of the five
//! availability predicates (10.11.z lines 337–438 — the fifth,
//! `IsHwTonemapAvailable`, lives beside its only caller in
//! [`super::input_args`]), `GetHwTonemapFilter` (3600–3680),
//! `GetLibplaceboFilter` (3681–3742), and the colour-property `setparams`
//! trio (6266–6303). The Dolby Vision / HDR10+ range classifiers (1368–1402)
//! come along because two of the predicates read them.
//!
//! There are **five** separate tonemapping paths, not one, and they are not
//! interchangeable — each is a different piece of silicon doing the work:
//!
//! | Path | Switch | Where it runs |
//! |---|---|---|
//! | software `tonemapx` | *(none)* | CPU, SIMD |
//! | OpenCL / CUDA `tonemap_*` | `EnableTonemapping` | GPU compute |
//! | `libplacebo` | `EnableTonemapping` | Vulkan |
//! | `vpp_qsv` / `tonemap_vaapi` | `EnableVppTonemapping` | Intel fixed function |
//! | `tonemap_videotoolbox` | `EnableVideoToolboxTonemapping` | Apple Metal |
//!
//! The software path is the odd one out: it has **no switch**, and turns on
//! from the source's range and the presence of the `tonemapx` filter alone.
//! Everything else is opt-in, which is why an HDR file plays washed out until
//! the operator finds the setting.

use std::fmt::Write as _;

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::data::{VideoRange, VideoRangeType};
use ferrofin_model::entities::{
    HardwareAccelerationType, TonemappingAlgorithm, TonemappingMode, TonemappingRange,
};
use ferrofin_model::entities_media::MediaStream;

use super::capabilities::{FfmpegCapabilities, Platform};
use super::decoder::{RequestedSize, fixed_output_size, is_size_fixed, video_color_bit_depth};
use super::versions::{
    MIN_FFMPEG_ADVANCED_TONEMAP_MODE, MIN_FFMPEG_OCL_CU_TONEMAP_MODE,
    MIN_FFMPEG_QSV_VPP_TONEMAP_OPTION,
};

/// The pixel format every hardware tonemap defaults to writing.
/// Port of the `videoFormat ?? "nv12"` fallbacks in `GetHwTonemapFilter`.
pub const DEFAULT_TONEMAP_FORMAT: &str = "nv12";

/// Whether the stream is Dolby Vision carrying an HDR10 base layer.
///
/// Port of `IsDoviWithHdr10Bl`. These are the profiles whose *base* layer is
/// ordinary HDR10, so a decoder that ignores the Dolby Vision RPU still gets a
/// usable HDR picture — which is what makes them tonemappable by hardware that
/// knows nothing about Dolby Vision.
#[must_use]
pub fn is_dovi_with_hdr10_bl(stream: Option<&MediaStream>) -> bool {
    matches!(
        stream.and_then(|s| s.video_range_type),
        Some(
            VideoRangeType::DoviWithHdr10
                | VideoRangeType::DoviWithEl
                | VideoRangeType::DoviWithHdr10Plus
                | VideoRangeType::DoviWithElhdr10Plus
                | VideoRangeType::DoviInvalid
        )
    )
}

/// Whether the stream carries Dolby Vision at all. Port of `IsDovi`.
#[must_use]
pub fn is_dovi(stream: Option<&MediaStream>) -> bool {
    is_dovi_with_hdr10_bl(stream)
        || matches!(
            stream.and_then(|s| s.video_range_type),
            Some(VideoRangeType::Dovi | VideoRangeType::DoviWithHlg | VideoRangeType::DoviWithSdr)
        )
}

/// Whether the stream carries HDR10+ dynamic metadata. Port of `IsHdr10Plus`.
#[must_use]
pub fn is_hdr10_plus(stream: Option<&MediaStream>) -> bool {
    matches!(
        stream.and_then(|s| s.video_range_type),
        Some(
            VideoRangeType::Hdr10Plus
                | VideoRangeType::DoviWithHdr10Plus
                | VideoRangeType::DoviWithElhdr10Plus
        )
    )
}

/// Whether the software `tonemapx` filter can run. Port of
/// `IsSwTonemapAvailable`.
///
/// Deliberately **not** gated on `EnableTonemapping` — upstream turns the
/// software path on from the source alone. `tonemapx` is a jellyfin-ffmpeg
/// patch, so a stock ffmpeg build simply does not have it.
#[must_use]
pub fn is_sw_tonemap_available(caps: &FfmpegCapabilities, stream: Option<&MediaStream>) -> bool {
    let Some(stream) = stream else {
        return false;
    };
    video_color_bit_depth(Some(stream)) >= 10
        && caps.supports_filter("tonemapx")
        && stream.video_range == Some(VideoRange::Hdr)
}

/// Whether the Vulkan `libplacebo` tonemap can run. Port of
/// `IsVulkanHwTonemapAvailable`.
///
/// Note the bit depth must be **exactly** 10, not "at least" — libplacebo has
/// only partial Dolby Vision support and upstream keeps it to the one depth it
/// has tested.
#[must_use]
pub fn is_vulkan_hw_tonemap_available(
    options: &EncodingOptions,
    stream: Option<&MediaStream>,
) -> bool {
    let Some(stream) = stream else {
        return false;
    };
    options.enable_tonemapping
        && stream.video_range == Some(VideoRange::Hdr)
        && video_color_bit_depth(Some(stream)) == 10
}

/// Whether Intel's fixed-function VPP tonemap can run. Port of
/// `IsIntelVppTonemapAvailable`.
///
/// The Windows/QSV version gate exists because `vpp_qsv` needs Intel's VPL
/// runtime, which only Gen12 (Tiger Lake) and newer have; on Linux upstream
/// prefers `tonemap_vaapi` so Gen9/Kaby Lake keeps working.
#[must_use]
pub fn is_intel_vpp_tonemap_available(
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    stream: Option<&MediaStream>,
) -> bool {
    let Some(stream) = stream else {
        return false;
    };
    if !options.enable_vpp_tonemapping || video_color_bit_depth(Some(stream)) < 10 {
        return false;
    }
    if caps.platform() == Platform::Windows
        && options.hardware_acceleration_type == HardwareAccelerationType::qsv
        && !caps.ffmpeg_at_least(MIN_FFMPEG_QSV_VPP_TONEMAP_OPTION)
    {
        return false;
    }
    stream.video_range == Some(VideoRange::Hdr)
        && (stream.video_range_type == Some(VideoRangeType::Hdr10)
            || is_hdr10_plus(Some(stream))
            || is_dovi_with_hdr10_bl(Some(stream)))
}

/// Whether Apple's Metal tonemap can run. Port of
/// `IsVideoToolboxTonemapAvailable`.
///
/// HLG is accepted here where the Intel VPP predicate rejects it, and Dolby
/// Vision profile 5 (`Dovi` with no HDR10 base layer) is excluded: it plays
/// correctly in Safari by direct play, but VideoToolbox maps it wrongly when
/// transcoding.
#[must_use]
pub fn is_videotoolbox_tonemap_available(
    options: &EncodingOptions,
    stream: Option<&MediaStream>,
) -> bool {
    let Some(stream) = stream else {
        return false;
    };
    if !options.enable_video_toolbox_tonemapping || video_color_bit_depth(Some(stream)) < 10 {
        return false;
    }
    stream.video_range == Some(VideoRange::Hdr)
        && (stream.video_range_type == Some(VideoRangeType::Hdr10)
            || is_hdr10_plus(Some(stream))
            || is_dovi_with_hdr10_bl(Some(stream))
            || stream.video_range_type == Some(VideoRangeType::Hlg))
}

/// The hardware tonemap filter for a backend suffix. Port of
/// `GetHwTonemapFilter`.
///
/// VAAPI is a different filter with a different vocabulary: `tonemap_vaapi` is
/// fixed-function silicon that takes **no** algorithm, peak, desaturation or
/// parameter — every one of those settings is silently ignored on that path —
/// and the only tuning it offers is a `procamp_vaapi` brightness/contrast pass
/// chained in front of it. Every other suffix (`opencl`, `cuda`,
/// `videotoolbox`) is the programmable `tonemap_*` filter and honours them all.
///
/// `force_full_range` is set when the encoder is MJPEG, which has no way to
/// signal a limited range.
#[must_use]
pub fn hw_tonemap_filter(
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    hw_tonemap_suffix: &str,
    video_format: Option<&str>,
    force_full_range: bool,
) -> String {
    if hw_tonemap_suffix.is_empty() {
        return String::new();
    }
    let format = video_format
        .filter(|f| !f.is_empty())
        .unwrap_or(DEFAULT_TONEMAP_FORMAT);
    let range = if force_full_range {
        TonemappingRange::pc
    } else {
        options.tonemapping_range
    };

    if hw_tonemap_suffix.eq_ignore_ascii_case("vaapi") {
        let brightness = options.vpp_tonemapping_brightness;
        let contrast = options.vpp_tonemapping_contrast;
        // Brightness is valid across [-100, 100] but 0 is "no change", so
        // upstream treats 0 as "not requested" rather than as a setting.
        let do_brightness = brightness != 0.0 && (-100.0..=100.0).contains(&brightness);
        // Contrast's neutral value is 1, so only above it counts as requested.
        let do_contrast = contrast > 1.0 && contrast <= 10.0;
        let procamp = match (do_brightness, do_contrast) {
            (true, true) => format!("procamp_vaapi=b={brightness}:c={contrast}"),
            (true, false) => format!("procamp_vaapi=b={brightness}"),
            (false, true) => format!("procamp_vaapi=c={contrast}"),
            (false, false) => String::new(),
        };
        let separator = if procamp.is_empty() { "" } else { "," };
        return format!(
            "{procamp}{separator}tonemap_vaapi=format={format}:p=bt709:t=bt709:m=bt709:\
             extra_hw_frames=32"
        );
    }

    let algorithm = lower(&format!("{:?}", options.tonemapping_algorithm));
    let mut args = format!(
        "tonemap_{hw_tonemap_suffix}=format={format}:p=bt709:t=bt709:m=bt709:\
         tonemap={algorithm}:peak={}:desat={}",
        options.tonemapping_peak, options.tonemapping_desat
    );

    // The two tonemap-mode families arrived in different ffmpeg releases.
    let mode = options.tonemapping_mode;
    let legacy_mode = matches!(mode, TonemappingMode::max | TonemappingMode::rgb)
        && caps.ffmpeg_at_least(MIN_FFMPEG_OCL_CU_TONEMAP_MODE);
    let advanced_mode = matches!(mode, TonemappingMode::lum | TonemappingMode::itp)
        && caps.ffmpeg_at_least(MIN_FFMPEG_ADVANCED_TONEMAP_MODE);
    if legacy_mode || advanced_mode {
        let _ = write!(args, ":tonemap_mode={}", lower(&format!("{mode:?}")));
    }

    if options.tonemapping_param != 0.0 {
        let _ = write!(args, ":param={}", options.tonemapping_param);
    }

    if matches!(range, TonemappingRange::tv | TonemappingRange::pc) {
        let _ = write!(args, ":range={}", lower(&format!("{range:?}")));
    }

    args
}

/// The `libplacebo` filter, which scales and tonemaps in one pass. Port of
/// `GetLibplaceboFilter`.
///
/// `upscaler=none:downscaler=none` is not "do not scale" — it tells libplacebo
/// to use its default samplers rather than an explicitly named one.
#[must_use]
pub fn libplacebo_filter(
    options: &EncodingOptions,
    video_format: Option<&str>,
    do_tonemap: bool,
    video_width: Option<i32>,
    video_height: Option<i32>,
    requested: RequestedSize,
    force_full_range: bool,
) -> String {
    let (out_width, out_height) = fixed_output_size(video_width, video_height, requested);
    let size_arg = match (out_width, out_height) {
        // The same rule `hw_scale_filter` applies, from the same function, so
        // the two cannot drift apart.
        (Some(w), Some(h)) if is_size_fixed(video_width, video_height, w, h) => {
            format!(":w={w}:h={h}")
        }
        _ => String::new(),
    };
    let format_arg = match video_format.filter(|f| !f.is_empty()) {
        Some(format) => format!(":format={format}"),
        None => String::new(),
    };

    let mut tonemap_arg = String::new();
    if do_tonemap {
        // libplacebo spells bt2390 with a dot, and "none" as "clip".
        let algorithm = match options.tonemapping_algorithm {
            TonemappingAlgorithm::bt2390 => "bt.2390".to_owned(),
            TonemappingAlgorithm::none => "clip".to_owned(),
            other => lower(&format!("{other:?}")),
        };
        tonemap_arg = format!(
            ":tonemapping={algorithm}:peak_detect=0:color_primaries=bt709:\
             color_trc=bt709:colorspace=bt709"
        );
        let range = if force_full_range {
            TonemappingRange::pc
        } else {
            options.tonemapping_range
        };
        if matches!(range, TonemappingRange::tv | TonemappingRange::pc) {
            let _ = write!(tonemap_arg, ":range={}", lower(&format!("{range:?}")));
        }
    }

    format!("libplacebo=upscaler=none:downscaler=none{size_arg}{format_arg}{tonemap_arg}")
}

/// The `setparams` filter that states what colour space the frames are in.
///
/// Port of `GetOverwriteColorPropertiesParam`. When a tonemap is going to run,
/// the *input* is described as HDR so the tonemapper knows what it is reading;
/// otherwise the *output* is declared SDR. Upstream forces this because a
/// stream's own colour metadata is often absent or wrong, and a tonemapper
/// given the wrong input primaries produces a picture that looks plausible and
/// is badly off.
#[must_use]
pub fn overwrite_color_properties_param(
    color_transfer: Option<&str>,
    is_tonemap_available: bool,
) -> String {
    if is_tonemap_available {
        input_hdr_param(color_transfer)
    } else {
        output_sdr_param(None)
    }
}

/// The `setparams` describing an HDR input. Port of `GetInputHdrParam`.
///
/// HLG and HDR10 differ only in the transfer function; both are BT.2020.
#[must_use]
pub fn input_hdr_param(color_transfer: Option<&str>) -> String {
    if color_transfer.is_some_and(|t| t.eq_ignore_ascii_case("arib-std-b67")) {
        // HLG
        "setparams=color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc".to_owned()
    } else {
        // HDR10
        "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc".to_owned()
    }
}

/// The `setparams` declaring an SDR output. Port of `GetOutputSdrParam`.
#[must_use]
pub fn output_sdr_param(tonemapping_range: Option<&str>) -> String {
    let base = "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";
    match tonemapping_range {
        Some(r) if r.eq_ignore_ascii_case("tv") => format!("{base}:range=tv"),
        Some(r) if r.eq_ignore_ascii_case("pc") => format!("{base}:range=pc"),
        _ => base.to_owned(),
    }
}

/// `enum.ToString().ToLowerInvariant()`, which is how C# spells every one of
/// these filter option values.
fn lower(value: &str) -> String {
    value.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::FfmpegVersion;
    use rstest::rstest;

    // Hand-derived from the C# (10.11.z 337-438, 1368-1402, 3600-3742,
    // 6266-6303). Upstream ships no tests for any of it.

    fn caps(version: FfmpegVersion, platform: Platform) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(platform)
            .filters(crate::encoder::REQUIRED_FILTERS)
            .ffmpeg_version(version)
            .build()
    }

    fn hdr(range_type: VideoRangeType) -> MediaStream {
        MediaStream {
            codec: Some("hevc".to_owned()),
            pixel_format: Some("yuv420p10le".to_owned()),
            video_range: Some(VideoRange::Hdr),
            video_range_type: Some(range_type),
            ..MediaStream::default()
        }
    }

    // ----- range classifiers -------------------------------------------------

    #[rstest]
    #[case(VideoRangeType::DoviWithHdr10, true, true, false)]
    #[case(VideoRangeType::DoviWithEl, true, true, false)]
    #[case(VideoRangeType::DoviWithHdr10Plus, true, true, true)]
    #[case(VideoRangeType::DoviWithElhdr10Plus, true, true, true)]
    #[case(VideoRangeType::DoviInvalid, true, true, false)]
    // Dolby Vision without an HDR10 base layer: still DOVI, no base layer.
    #[case(VideoRangeType::Dovi, false, true, false)]
    #[case(VideoRangeType::DoviWithHlg, false, true, false)]
    #[case(VideoRangeType::DoviWithSdr, false, true, false)]
    // Plain HDR flavours are neither.
    #[case(VideoRangeType::Hdr10, false, false, false)]
    #[case(VideoRangeType::Hlg, false, false, false)]
    #[case(VideoRangeType::Hdr10Plus, false, false, true)]
    #[case(VideoRangeType::Sdr, false, false, false)]
    fn the_range_classifiers_partition_the_dolby_vision_profiles(
        #[case] range_type: VideoRangeType,
        #[case] with_hdr10_bl: bool,
        #[case] dovi: bool,
        #[case] hdr10_plus: bool,
    ) {
        let s = hdr(range_type);
        assert_eq!(
            is_dovi_with_hdr10_bl(Some(&s)),
            with_hdr10_bl,
            "{range_type:?}"
        );
        assert_eq!(is_dovi(Some(&s)), dovi, "{range_type:?}");
        assert_eq!(is_hdr10_plus(Some(&s)), hdr10_plus, "{range_type:?}");
    }

    #[test]
    fn no_stream_is_no_range() {
        assert!(!is_dovi_with_hdr10_bl(None));
        assert!(!is_dovi(None));
        assert!(!is_hdr10_plus(None));
    }

    // ----- availability ------------------------------------------------------

    #[test]
    fn the_software_tonemap_has_no_setting_to_turn_on() {
        // The one path that is not opt-in: upstream decides it from the source
        // and the presence of the jellyfin-ffmpeg `tonemapx` filter alone.
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        let s = hdr(VideoRangeType::Hdr10);
        assert!(is_sw_tonemap_available(&caps, Some(&s)));

        // A build without the patched filter cannot.
        let stock = FfmpegCapabilities::builder()
            .filters(
                crate::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "tonemapx"),
            )
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .build();
        assert!(!is_sw_tonemap_available(&stock, Some(&s)));

        // 8-bit or SDR sources have nothing to map.
        let mut eight = s.clone();
        eight.pixel_format = Some("yuv420p".to_owned());
        assert!(!is_sw_tonemap_available(&caps, Some(&eight)));
        let mut sdr = s.clone();
        sdr.video_range = Some(VideoRange::Sdr);
        assert!(!is_sw_tonemap_available(&caps, Some(&sdr)));
        assert!(!is_sw_tonemap_available(&caps, None));
    }

    #[test]
    fn libplacebo_needs_exactly_ten_bits() {
        let mut options = EncodingOptions {
            enable_tonemapping: true,
            ..EncodingOptions::default()
        };
        let ten = hdr(VideoRangeType::Hdr10);
        assert!(is_vulkan_hw_tonemap_available(&options, Some(&ten)));

        // Twelve bits is not "at least ten" here — upstream pins the one depth
        // it has tested.
        let mut twelve = ten.clone();
        twelve.pixel_format = Some("yuv420p12le".to_owned());
        assert!(!is_vulkan_hw_tonemap_available(&options, Some(&twelve)));

        options.enable_tonemapping = false;
        assert!(!is_vulkan_hw_tonemap_available(&options, Some(&ten)));
    }

    #[rstest]
    // The Intel VPP path takes HDR10, HDR10+, and Dolby Vision with an HDR10
    // base layer...
    #[case(VideoRangeType::Hdr10, true)]
    #[case(VideoRangeType::Hdr10Plus, true)]
    #[case(VideoRangeType::DoviWithHdr10, true)]
    // ...but not HLG, and not Dolby Vision without a base layer.
    #[case(VideoRangeType::Hlg, false)]
    #[case(VideoRangeType::Dovi, false)]
    fn the_intel_vpp_tonemap_takes_only_hdr10_shaped_sources(
        #[case] range_type: VideoRangeType,
        #[case] expected: bool,
    ) {
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        let options = EncodingOptions {
            enable_vpp_tonemapping: true,
            ..EncodingOptions::default()
        };
        assert_eq!(
            is_intel_vpp_tonemap_available(&caps, &options, Some(&hdr(range_type))),
            expected,
            "{range_type:?}"
        );
    }

    #[test]
    fn qsv_vpp_tonemapping_on_windows_needs_ffmpeg_7_0_1() {
        // `vpp_qsv` needs Intel's VPL runtime; on Linux upstream prefers
        // `tonemap_vaapi` so Gen9/Kaby Lake keeps working, and applies no gate.
        let options = EncodingOptions {
            enable_vpp_tonemapping: true,
            hardware_acceleration_type: HardwareAccelerationType::qsv,
            ..EncodingOptions::default()
        };
        let s = hdr(VideoRangeType::Hdr10);

        let old_windows = caps(FfmpegVersion::new(6, 0), Platform::Windows);
        assert!(!is_intel_vpp_tonemap_available(
            &old_windows,
            &options,
            Some(&s)
        ));

        let new_windows = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Windows);
        assert!(is_intel_vpp_tonemap_available(
            &new_windows,
            &options,
            Some(&s)
        ));

        // The same old ffmpeg on Linux is fine.
        let old_linux = caps(FfmpegVersion::new(6, 0), Platform::Linux);
        assert!(is_intel_vpp_tonemap_available(
            &old_linux,
            &options,
            Some(&s)
        ));
    }

    #[rstest]
    // VideoToolbox additionally accepts HLG...
    #[case(VideoRangeType::Hdr10, true)]
    #[case(VideoRangeType::Hlg, true)]
    #[case(VideoRangeType::Hdr10Plus, true)]
    #[case(VideoRangeType::DoviWithHdr10, true)]
    // ...but excludes Dolby Vision profile 5, which it maps incorrectly when
    // transcoding even though Safari direct-plays it correctly.
    #[case(VideoRangeType::Dovi, false)]
    fn the_videotoolbox_tonemap_adds_hlg_and_drops_profile_5(
        #[case] range_type: VideoRangeType,
        #[case] expected: bool,
    ) {
        let options = EncodingOptions {
            enable_video_toolbox_tonemapping: true,
            ..EncodingOptions::default()
        };
        assert_eq!(
            is_videotoolbox_tonemap_available(&options, Some(&hdr(range_type))),
            expected,
            "{range_type:?}"
        );
    }

    // ----- the filters themselves --------------------------------------------

    #[test]
    fn the_programmable_tonemap_carries_every_setting() {
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        let options = EncodingOptions::default();
        assert_eq!(
            hw_tonemap_filter(&caps, &options, "opencl", None, false),
            "tonemap_opencl=format=nv12:p=bt709:t=bt709:m=bt709:tonemap=bt2390:peak=100:desat=0"
        );
        // A named format replaces the nv12 default.
        assert_eq!(
            hw_tonemap_filter(&caps, &options, "cuda", Some("yuv420p"), false),
            "tonemap_cuda=format=yuv420p:p=bt709:t=bt709:m=bt709:tonemap=bt2390:peak=100:desat=0"
        );
        assert_eq!(hw_tonemap_filter(&caps, &options, "", None, false), "");
    }

    #[test]
    fn the_tonemap_mode_families_arrived_in_different_ffmpeg_releases() {
        // A helper so each variation is a fresh value rather than a mutation.
        let with_mode = |mode| EncodingOptions {
            tonemapping_mode: mode,
            ..EncodingOptions::default()
        };
        // The legacy pair needs 5.1.3.
        let options = with_mode(TonemappingMode::rgb);
        let old = caps(FfmpegVersion::new(5, 1), Platform::Linux);
        assert!(!hw_tonemap_filter(&old, &options, "opencl", None, false).contains("tonemap_mode"));
        let mid = caps(FfmpegVersion::with_build(5, 1, 3), Platform::Linux);
        assert!(
            hw_tonemap_filter(&mid, &options, "opencl", None, false).contains(":tonemap_mode=rgb")
        );

        // The advanced pair needs 7.0.1, so 5.1.3 is not enough for them.
        let options = with_mode(TonemappingMode::itp);
        assert!(!hw_tonemap_filter(&mid, &options, "opencl", None, false).contains("tonemap_mode"));
        let new = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        assert!(
            hw_tonemap_filter(&new, &options, "opencl", None, false).contains(":tonemap_mode=itp")
        );

        // `auto` is in neither family and is never emitted.
        let options = with_mode(TonemappingMode::auto);
        assert!(!hw_tonemap_filter(&new, &options, "opencl", None, false).contains("tonemap_mode"));
    }

    #[test]
    fn the_optional_tonemap_arguments_appear_only_when_set() {
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        let mut options = EncodingOptions::default();

        // param defaults to 0, which means "unset" rather than "zero".
        assert!(!hw_tonemap_filter(&caps, &options, "opencl", None, false).contains(":param="));
        options.tonemapping_param = 0.5;
        assert!(hw_tonemap_filter(&caps, &options, "opencl", None, false).contains(":param=0.5"));

        // range defaults to `auto`, which emits nothing.
        assert!(!hw_tonemap_filter(&caps, &options, "opencl", None, false).contains(":range="));
        options.tonemapping_range = TonemappingRange::tv;
        assert!(hw_tonemap_filter(&caps, &options, "opencl", None, false).contains(":range=tv"));

        // An MJPEG encoder forces full range regardless of the setting.
        assert!(hw_tonemap_filter(&caps, &options, "opencl", None, true).contains(":range=pc"));
    }

    #[test]
    fn the_vaapi_tonemap_ignores_every_programmable_setting() {
        // `tonemap_vaapi` is fixed-function: no algorithm, peak, desaturation,
        // parameter or mode reaches it, however they are configured.
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        let mut options = EncodingOptions {
            tonemapping_algorithm: TonemappingAlgorithm::reinhard,
            tonemapping_peak: 400.0,
            tonemapping_desat: 0.5,
            tonemapping_param: 0.7,
            tonemapping_mode: TonemappingMode::rgb,
            ..EncodingOptions::default()
        };
        // Neutralise procamp so this test is purely about the programmable
        // settings. (Brightness DEFAULTS to 16, which is non-zero, so the
        // shipped configuration really does chain a procamp pass — that is
        // what `the_vaapi_tonemap_chains_procamp_when_asked` covers.)
        options.vpp_tonemapping_brightness = 0.0;
        options.vpp_tonemapping_contrast = 1.0;

        let filter = hw_tonemap_filter(&caps, &options, "vaapi", None, false);
        assert_eq!(
            filter,
            "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:extra_hw_frames=32"
        );
        for ignored in ["reinhard", "peak", "desat", "param", "tonemap_mode"] {
            assert!(!filter.contains(ignored), "{ignored} leaked into {filter}");
        }
    }

    #[test]
    fn the_vaapi_tonemap_chains_procamp_when_asked() {
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        // A helper so each variation is a fresh value rather than a mutation.
        let procamp = |brightness, contrast| EncodingOptions {
            vpp_tonemapping_brightness: brightness,
            vpp_tonemapping_contrast: contrast,
            ..EncodingOptions::default()
        };

        // Brightness alone. (Its default of 16 is already non-zero, so the
        // shipped configuration does chain procamp.)
        let options = procamp(20.0, 1.0);
        assert_eq!(
            hw_tonemap_filter(&caps, &options, "vaapi", None, false),
            "procamp_vaapi=b=20,tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:\
             extra_hw_frames=32"
        );

        // Contrast alone.
        let options = procamp(0.0, 2.0);
        assert_eq!(
            hw_tonemap_filter(&caps, &options, "vaapi", None, false),
            "procamp_vaapi=c=2,tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:\
             extra_hw_frames=32"
        );

        // Both, joined by a colon inside the one procamp filter.
        let options = procamp(20.0, 2.0);
        assert_eq!(
            hw_tonemap_filter(&caps, &options, "vaapi", None, false),
            "procamp_vaapi=b=20:c=2,tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:\
             extra_hw_frames=32"
        );

        // Out-of-range values are ignored rather than clamped.
        let options = procamp(200.0, 50.0);
        assert_eq!(
            hw_tonemap_filter(&caps, &options, "vaapi", None, false),
            "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:extra_hw_frames=32"
        );

        // The bounds are INCLUSIVE at both ends: brightness [-100, 100] and
        // contrast (1, 10]. One step outside and the setting is dropped.
        for (brightness, contrast, expected_procamp) in [
            (100.0, 10.0, "procamp_vaapi=b=100:c=10,"),
            (-100.0, 10.0, "procamp_vaapi=b=-100:c=10,"),
            (100.1, 10.0, "procamp_vaapi=c=10,"),
            (-100.1, 10.0, "procamp_vaapi=c=10,"),
            (100.0, 10.1, "procamp_vaapi=b=100,"),
            // Contrast's neutral value is 1, so exactly 1 is "not requested".
            (100.0, 1.0, "procamp_vaapi=b=100,"),
        ] {
            let filter =
                hw_tonemap_filter(&caps, &procamp(brightness, contrast), "vaapi", None, false);
            assert!(
                filter.starts_with(expected_procamp),
                "b={brightness} c={contrast} gave {filter}"
            );
        }
    }

    #[test]
    fn the_shipped_vaapi_defaults_already_chain_a_procamp_pass() {
        // `VppTonemappingBrightness` defaults to 16, which is non-zero, so an
        // operator who changes nothing still gets a procamp filter. Pinning the
        // out-of-the-box string keeps that visible.
        let caps = caps(FfmpegVersion::with_build(7, 0, 1), Platform::Linux);
        assert_eq!(
            hw_tonemap_filter(&caps, &EncodingOptions::default(), "vaapi", None, false),
            "procamp_vaapi=b=16,tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:\
             extra_hw_frames=32"
        );
    }

    #[test]
    fn libplacebo_scales_and_tonemaps_in_one_filter() {
        let options = EncodingOptions::default();
        let requested = RequestedSize {
            width: Some(1280),
            height: Some(720),
            ..RequestedSize::default()
        };
        assert_eq!(
            libplacebo_filter(
                &options,
                Some("nv12"),
                false,
                Some(1920),
                Some(1080),
                requested,
                false
            ),
            "libplacebo=upscaler=none:downscaler=none:w=1280:h=720:format=nv12"
        );
        assert_eq!(
            libplacebo_filter(
                &options,
                Some("nv12"),
                true,
                Some(1920),
                Some(1080),
                requested,
                false
            ),
            "libplacebo=upscaler=none:downscaler=none:w=1280:h=720:format=nv12:\
             tonemapping=bt.2390:peak_detect=0:color_primaries=bt709:color_trc=bt709:\
             colorspace=bt709"
        );
        // Nothing fixed at all: the bare filter. A same-size job must NOT
        // restate the dimensions.
        assert_eq!(
            libplacebo_filter(
                &options,
                None,
                false,
                Some(1920),
                Some(1080),
                RequestedSize {
                    width: Some(1920),
                    height: Some(1080),
                    ..RequestedSize::default()
                },
                false
            ),
            "libplacebo=upscaler=none:downscaler=none"
        );
        // ...and an unknown source size has nothing to compare, so the output
        // dimensions are stated.
        assert_eq!(
            libplacebo_filter(&options, None, false, None, None, requested, false),
            "libplacebo=upscaler=none:downscaler=none:w=1280:h=720"
        );
    }

    #[rstest]
    // libplacebo spells the default algorithm with a dot...
    #[case(TonemappingAlgorithm::bt2390, "bt.2390")]
    // ...and "no tonemapping" as clipping.
    #[case(TonemappingAlgorithm::none, "clip")]
    #[case(TonemappingAlgorithm::reinhard, "reinhard")]
    #[case(TonemappingAlgorithm::hable, "hable")]
    fn libplacebo_has_its_own_algorithm_spellings(
        #[case] algorithm: TonemappingAlgorithm,
        #[case] expected: &str,
    ) {
        let options = EncodingOptions {
            tonemapping_algorithm: algorithm,
            ..EncodingOptions::default()
        };
        let filter = libplacebo_filter(
            &options,
            None,
            true,
            None,
            None,
            RequestedSize::default(),
            false,
        );
        assert!(
            filter.contains(&format!(":tonemapping={expected}:")),
            "{filter}"
        );
    }

    #[test]
    fn libplacebo_takes_the_range_like_the_other_tonemaps() {
        let options = EncodingOptions {
            tonemapping_range: TonemappingRange::tv,
            ..EncodingOptions::default()
        };
        assert!(
            libplacebo_filter(
                &options,
                None,
                true,
                None,
                None,
                RequestedSize::default(),
                false
            )
            .ends_with(":range=tv")
        );
        // Forced full range wins over the setting.
        assert!(
            libplacebo_filter(
                &options,
                None,
                true,
                None,
                None,
                RequestedSize::default(),
                true
            )
            .ends_with(":range=pc")
        );
        // And no range at all when not tonemapping.
        assert!(
            !libplacebo_filter(
                &options,
                None,
                false,
                None,
                None,
                RequestedSize::default(),
                false
            )
            .contains("range")
        );
    }

    // ----- the colour-property setparams -------------------------------------

    #[test]
    fn the_colour_properties_describe_the_input_when_tonemapping() {
        // Tonemapping: state what the frames ARE, so the tonemapper reads them
        // correctly.
        assert_eq!(
            overwrite_color_properties_param(Some("smpte2084"), true),
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc"
        );
        // HLG carries a different transfer function, same primaries.
        assert_eq!(
            overwrite_color_properties_param(Some("arib-std-b67"), true),
            "setparams=color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc"
        );
        // An unknown transfer is assumed HDR10.
        assert_eq!(
            overwrite_color_properties_param(None, true),
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc"
        );
        // Not tonemapping: declare the OUTPUT as SDR.
        assert_eq!(
            overwrite_color_properties_param(Some("arib-std-b67"), false),
            "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709"
        );
    }

    #[rstest]
    #[case(
        None,
        "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709"
    )]
    #[case(
        Some("tv"),
        "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv"
    )]
    #[case(
        Some("pc"),
        "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=pc"
    )]
    // `auto` is not a range ffmpeg understands here, so nothing is appended.
    #[case(
        Some("auto"),
        "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709"
    )]
    fn the_sdr_output_params_take_an_optional_range(
        #[case] range: Option<&str>,
        #[case] expected: &str,
    ) {
        assert_eq!(output_sdr_param(range), expected);
    }
}
