//! "Is this backend usable?" — the capability predicates every hardware branch
//! consults before it emits a single argument.
//!
//! Port of the `Is*Supported` / `Is*FullSupported` methods of C#
//! `EncodingHelper` (10.11.z lines 261–336). Each asks the same shape of
//! question: does the running ffmpeg carry the hwaccel *and* every filter that
//! backend's chain will reach for? A build that has `scale_vaapi` but not
//! `tonemap_vaapi` cannot run the VAAPI chain, so it must be rejected up front
//! rather than half-way through building a filtergraph.
//!
//! Three upstream quirks are preserved deliberately, each marked in the C# with
//! its own "optional for the time being" comment: `transpose_opencl`,
//! `transpose_cuda` and `transpose_vt` are **not** required by their respective
//! "full support" checks, even though the chains use them — upstream keeps them
//! optional and simply omits the rotation step on a build that lacks them.

use super::capabilities::{FfmpegCapabilities, FilterOption};

/// Whether VAAPI can be used at all for this source codec.
///
/// Port of `IsVaapiSupported`. The `mpeg4` rejection is upstream's workaround
/// for a hard ffmpeg failure — `No VAAPI support for codec mpeg4 profile -99`
/// — not a capability question, which is why the source codec is a parameter.
#[must_use]
pub fn is_vaapi_supported(caps: &FfmpegCapabilities, video_codec: Option<&str>) -> bool {
    if video_codec.is_some_and(|codec| codec.eq_ignore_ascii_case("mpeg4")) {
        return false;
    }
    caps.supports_hwaccel("vaapi")
}

/// Whether the full VAAPI filter chain can run. Port of `IsVaapiFullSupported`.
#[must_use]
pub fn is_vaapi_full_supported(caps: &FfmpegCapabilities) -> bool {
    caps.supports_hwaccel("drm")
        && caps.supports_hwaccel("vaapi")
        && caps.supports_filter("scale_vaapi")
        && caps.supports_filter("deinterlace_vaapi")
        && caps.supports_filter("tonemap_vaapi")
        && caps.supports_filter("procamp_vaapi")
        && caps.supports_filter_with_option(FilterOption::OverlayVaapiFrameSync)
        && caps.supports_filter("transpose_vaapi")
        && caps.supports_filter("hwupload_vaapi")
}

/// Whether the full Rockchip RGA filter chain can run. Port of
/// `IsRkmppFullSupported`.
#[must_use]
pub fn is_rkmpp_full_supported(caps: &FfmpegCapabilities) -> bool {
    caps.supports_hwaccel("rkmpp")
        && caps.supports_filter("scale_rkrga")
        && caps.supports_filter("vpp_rkrga")
        && caps.supports_filter("overlay_rkrga")
}

/// Whether the OpenCL filter chain can run. Port of `IsOpenclFullSupported`.
///
/// `transpose_opencl` is deliberately not required — see the module docs.
#[must_use]
pub fn is_opencl_full_supported(caps: &FfmpegCapabilities) -> bool {
    caps.supports_hwaccel("opencl")
        && caps.supports_filter("scale_opencl")
        && caps.supports_filter_with_option(FilterOption::TonemapOpenclBt2390)
        && caps.supports_filter_with_option(FilterOption::OverlayOpenclFrameSync)
}

/// Whether the CUDA filter chain can run. Port of `IsCudaFullSupported`.
///
/// `transpose_cuda` is deliberately not required — see the module docs. Note
/// `scale_cuda` and `tonemap_cuda` are checked by *option*, not presence: some
/// builds ship a `scale_cuda` without `format` and a `tonemap_cuda` that is not
/// the real GPU tonemap.
#[must_use]
pub fn is_cuda_full_supported(caps: &FfmpegCapabilities) -> bool {
    caps.supports_hwaccel("cuda")
        && caps.supports_filter_with_option(FilterOption::ScaleCudaFormat)
        && caps.supports_filter("yadif_cuda")
        && caps.supports_filter_with_option(FilterOption::TonemapCudaName)
        && caps.supports_filter("overlay_cuda")
        && caps.supports_filter("hwupload_cuda")
}

/// Whether the Vulkan/libplacebo filter chain can run. Port of
/// `IsVulkanFullSupported`.
#[must_use]
pub fn is_vulkan_full_supported(caps: &FfmpegCapabilities) -> bool {
    caps.supports_hwaccel("vulkan")
        && caps.supports_filter("libplacebo")
        && caps.supports_filter("scale_vulkan")
        && caps.supports_filter_with_option(FilterOption::OverlayVulkanFrameSync)
        && caps.supports_filter("transpose_vulkan")
        && caps.supports_filter("flip_vulkan")
}

/// Whether the VideoToolbox filter chain can run. Port of
/// `IsVideoToolboxFullSupported`.
///
/// `transpose_vt` is deliberately not required — see the module docs.
#[must_use]
pub fn is_videotoolbox_full_supported(caps: &FfmpegCapabilities) -> bool {
    caps.supports_hwaccel("videotoolbox")
        && caps.supports_filter("yadif_videotoolbox")
        && caps.supports_filter("overlay_videotoolbox")
        && caps.supports_filter("tonemap_videotoolbox")
        && caps.supports_filter("scale_vt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Every hwaccel any predicate here asks about.
    const ALL_HWACCELS: [&str; 9] = [
        "drm",
        "vaapi",
        "rkmpp",
        "opencl",
        "cuda",
        "vulkan",
        "videotoolbox",
        "qsv",
        "d3d11va",
    ];

    /// A capability set carrying every hwaccel, filter and filter option, so a
    /// test can remove exactly the one it is about.
    fn everything() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .hwaccels(ALL_HWACCELS)
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .build()
    }

    /// `everything()` minus one filter.
    fn without_filter(name: &str) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .hwaccels(ALL_HWACCELS)
            .filters(
                crate::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != name),
            )
            .all_filter_options(true)
            .build()
    }

    /// `everything()` minus one hwaccel.
    fn without_hwaccel(name: &str) -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .hwaccels(ALL_HWACCELS.into_iter().filter(|h| *h != name))
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .build()
    }

    #[test]
    fn a_fully_equipped_build_supports_every_backend() {
        let caps = everything();
        assert!(is_vaapi_supported(&caps, Some("h264")));
        assert!(is_vaapi_full_supported(&caps));
        assert!(is_rkmpp_full_supported(&caps));
        assert!(is_opencl_full_supported(&caps));
        assert!(is_cuda_full_supported(&caps));
        assert!(is_vulkan_full_supported(&caps));
        assert!(is_videotoolbox_full_supported(&caps));
    }

    #[test]
    fn a_bare_build_supports_nothing() {
        let caps = FfmpegCapabilities::default();
        assert!(!is_vaapi_supported(&caps, Some("h264")));
        assert!(!is_vaapi_full_supported(&caps));
        assert!(!is_rkmpp_full_supported(&caps));
        assert!(!is_opencl_full_supported(&caps));
        assert!(!is_cuda_full_supported(&caps));
        assert!(!is_vulkan_full_supported(&caps));
        assert!(!is_videotoolbox_full_supported(&caps));
    }

    /// What one backend's "full support" predicate requires.
    struct Requirements {
        predicate: fn(&FfmpegCapabilities) -> bool,
        hwaccels: &'static [&'static str],
        filters: &'static [&'static str],
    }

    /// Every predicate, paired with the filters and hwaccels its C# conjunction
    /// requires. Dropping any one of them must disqualify that backend — this
    /// is the table that stops a conjunct from being deleted unnoticed.
    fn required_by(name: &str) -> Requirements {
        match name {
            "vaapi" => Requirements {
                predicate: is_vaapi_full_supported,
                hwaccels: &["drm", "vaapi"],
                filters: &[
                    "scale_vaapi",
                    "deinterlace_vaapi",
                    "tonemap_vaapi",
                    "procamp_vaapi",
                    "transpose_vaapi",
                    "hwupload_vaapi",
                ],
            },
            "rkmpp" => Requirements {
                predicate: is_rkmpp_full_supported,
                hwaccels: &["rkmpp"],
                filters: &["scale_rkrga", "vpp_rkrga", "overlay_rkrga"],
            },
            "opencl" => Requirements {
                predicate: is_opencl_full_supported,
                hwaccels: &["opencl"],
                filters: &["scale_opencl"],
            },
            "cuda" => Requirements {
                predicate: is_cuda_full_supported,
                hwaccels: &["cuda"],
                filters: &["yadif_cuda", "overlay_cuda", "hwupload_cuda"],
            },
            "vulkan" => Requirements {
                predicate: is_vulkan_full_supported,
                hwaccels: &["vulkan"],
                filters: &[
                    "libplacebo",
                    "scale_vulkan",
                    "transpose_vulkan",
                    "flip_vulkan",
                ],
            },
            "videotoolbox" => Requirements {
                predicate: is_videotoolbox_full_supported,
                hwaccels: &["videotoolbox"],
                filters: &[
                    "yadif_videotoolbox",
                    "overlay_videotoolbox",
                    "tonemap_videotoolbox",
                    "scale_vt",
                ],
            },
            other => panic!("unknown backend {other}"),
        }
    }

    #[rstest]
    #[case("vaapi")]
    #[case("rkmpp")]
    #[case("opencl")]
    #[case("cuda")]
    #[case("vulkan")]
    #[case("videotoolbox")]
    fn every_required_filter_is_load_bearing(#[case] backend: &str) {
        let req = required_by(backend);
        assert!(
            (req.predicate)(&everything()),
            "{backend} should start supported"
        );
        for filter in req.filters {
            assert!(
                !(req.predicate)(&without_filter(filter)),
                "{backend}: dropping {filter} must disqualify it"
            );
        }
    }

    #[rstest]
    #[case("vaapi")]
    #[case("rkmpp")]
    #[case("opencl")]
    #[case("cuda")]
    #[case("vulkan")]
    #[case("videotoolbox")]
    fn every_required_hwaccel_is_load_bearing(#[case] backend: &str) {
        let req = required_by(backend);
        for hwaccel in req.hwaccels {
            assert!(
                !(req.predicate)(&without_hwaccel(hwaccel)),
                "{backend}: dropping the {hwaccel} hwaccel must disqualify it"
            );
        }
    }

    #[rstest]
    // Each filter option, against the predicate that requires it.
    #[case(FilterOption::OverlayVaapiFrameSync, "vaapi")]
    #[case(FilterOption::TonemapOpenclBt2390, "opencl")]
    #[case(FilterOption::OverlayOpenclFrameSync, "opencl")]
    #[case(FilterOption::ScaleCudaFormat, "cuda")]
    #[case(FilterOption::TonemapCudaName, "cuda")]
    #[case(FilterOption::OverlayVulkanFrameSync, "vulkan")]
    fn every_required_filter_option_is_load_bearing(
        #[case] option: FilterOption,
        #[case] backend: &str,
    ) {
        let req = required_by(backend);
        let caps = FfmpegCapabilities::builder()
            .hwaccels(ALL_HWACCELS)
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(true)
            .filter_option(option, false)
            .build();
        assert!(
            !(req.predicate)(&caps),
            "{backend}: dropping {option:?} must disqualify it"
        );
    }

    #[test]
    fn the_optional_transpose_filters_are_genuinely_optional() {
        // Upstream marks each of these "optional for the time being" and omits
        // the rotation step rather than dropping to software. Pinning it here
        // stops a future tidy-up from quietly making them required, which would
        // push whole classes of build back onto the CPU.
        assert!(is_opencl_full_supported(&without_filter(
            "transpose_opencl"
        )));
        assert!(is_cuda_full_supported(&without_filter("transpose_cuda")));
        assert!(is_videotoolbox_full_supported(&without_filter(
            "transpose_vt"
        )));
    }

    #[test]
    fn cuda_and_opencl_check_options_not_just_filter_presence() {
        // A build can list `scale_cuda` without the `format` option, or a
        // `tonemap_cuda` that is not the real GPU tonemap. Presence alone must
        // not be enough.
        let caps = FfmpegCapabilities::builder()
            .hwaccels(["cuda", "opencl"])
            .filters(crate::encoder::REQUIRED_FILTERS)
            .all_filter_options(false)
            .build();
        assert!(!is_cuda_full_supported(&caps));
        assert!(!is_opencl_full_supported(&caps));

        for option in [FilterOption::ScaleCudaFormat, FilterOption::TonemapCudaName] {
            let caps = FfmpegCapabilities::builder()
                .hwaccels(["cuda"])
                .filters(crate::encoder::REQUIRED_FILTERS)
                .all_filter_options(true)
                .filter_option(option, false)
                .build();
            assert!(
                !is_cuda_full_supported(&caps),
                "{option:?} must be required for CUDA"
            );
        }
    }

    #[test]
    fn vaapi_rejects_mpeg4_regardless_of_capabilities() {
        // Not a capability question: ffmpeg hard-fails on VAAPI + mpeg4.
        let caps = everything();
        assert!(!is_vaapi_supported(&caps, Some("mpeg4")));
        assert!(!is_vaapi_supported(&caps, Some("MPEG4")));
        // A source with no known codec is not the mpeg4 case, so it passes the
        // codec test and falls through to the hwaccel test, as C# does.
        assert!(is_vaapi_supported(&caps, None));
        assert!(!is_vaapi_supported(&FfmpegCapabilities::default(), None));
    }
}
