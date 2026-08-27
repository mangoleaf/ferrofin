//! Port of `MediaBrowser.MediaEncoding.Encoder.EncoderValidator`.
//!
//! Everything here is *pure*: each function takes the text an ffmpeg probe
//! produced and returns the decision C# reaches from the same text. The
//! spawning half — which flags to run, and running them concurrently at
//! startup — belongs to the composition root, because a process spawn is not
//! unit-testable and the capability probes are all one-shot startup reads.
//!
//! Covered: the `ffmpeg -version` parse and validation
//! ([`EncoderValidator::get_ffmpeg_version_internal`],
//! [`EncoderValidator::validate_version_internal`]), the
//! `-encoders`/`-decoders`/`-filters`/`-hwaccels` enumerations, the
//! `-h filter=…` / `-h bsf=…` option probes, and the two Linux device probes
//! (VAAPI driver name, Vulkan DRM extensions) that read ffmpeg's *stderr*.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::version::FfmpegVersion;
use crate::encoding_helper::hw::{BsfOption, FilterOption};

/// The minimum recommended ffmpeg version (`4.4`).
///
/// When changing this, also change [`FFMPEG_MINIMUM_LIBRARY_VERSIONS`].
pub const MIN_VERSION: FfmpegVersion = FfmpegVersion::new(4, 4);

/// The maximum recommended ffmpeg version (unbounded — C# `MaxVersion` is `null`).
pub const MAX_VERSION: Option<FfmpegVersion> = None;

/// `^ffmpeg version n?((?:[0-9]+\.?)+)` — extracts the version from the first line.
static FFMPEG_VERSION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ffmpeg version n?((?:[0-9]+\.?)+)").expect("valid regex"));

/// `((?<name>lib\w+)\s+(?<major>[0-9]+)\.\s*(?<minor>[0-9]+))` (multiline) — matches
/// each `libavcodec 58.134` style line.
static LIBRARY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)((?P<name>lib\w+)\s+(?P<major>[0-9]+)\.\s*(?P<minor>[0-9]+))")
        .expect("valid regex")
});

/// `^\s\S{2,3}\s(?<filter>[\w|-]+)\s+.+$` (multiline) — matches each filter
/// name in `ffmpeg -filters` output (one leading space, the 2–3 char capability
/// flags, the name). The legend lines are indented two spaces and never match.
static FILTER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s\S{2,3}\s(?P<filter>[\w|-]+)\s+.+$").expect("valid regex")
});

/// `^\s\S{6}\s(?<codec>[\w|-]+)\s+.+$` (multiline) — matches each codec name in
/// `ffmpeg -encoders` / `-decoders` output (one leading space, the 6-char
/// capability flags, the name). The legend and `------` divider never match.
static CODEC_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s\S{6}\s(?P<codec>[\w|-]+)\s+.+$").expect("valid regex"));

/// The library versions corresponding to the minimum ffmpeg version 4.4.
///
/// Refers to the versions in <https://ffmpeg.org/download.html>. Used to work
/// out the ffmpeg version when the version string is missing from the output.
static FFMPEG_MINIMUM_LIBRARY_VERSIONS: LazyLock<HashMap<&'static str, FfmpegVersion>> =
    LazyLock::new(|| {
        HashMap::from([
            ("libavutil", FfmpegVersion::new(56, 70)),
            ("libavcodec", FfmpegVersion::new(58, 134)),
            ("libavformat", FfmpegVersion::new(58, 76)),
            ("libavdevice", FfmpegVersion::new(58, 13)),
            ("libavfilter", FfmpegVersion::new(7, 110)),
            ("libswscale", FfmpegVersion::new(5, 9)),
            ("libswresample", FfmpegVersion::new(3, 9)),
        ])
    });

/// The decoders Jellyfin enumerates. Port of C# `_requiredDecoders`.
///
/// `GetCodecs` intersects the parsed `-decoders` output with this list, so a
/// decoder absent here reads as unsupported no matter what ffmpeg reports.
/// That is deliberate upstream — the list is the set of decoders any decision
/// actually consults — and it keeps the retained capability lists small. Adding
/// a decoder to a decision means adding it here too.
///
/// There are no `*_vaapi` or `*_amf` entries: those are hwaccel-only, selected
/// with `-hwaccel` rather than a named `-c:v` decoder.
pub const REQUIRED_DECODERS: [&str; 41] = [
    "h264",
    "hevc",
    "vp8",
    "libvpx",
    "vp9",
    "libvpx-vp9",
    "av1",
    "libdav1d",
    "mpeg2video",
    "mpeg4",
    "msmpeg4",
    "dca",
    "ac3",
    "ac4",
    "aac",
    "mp3",
    "flac",
    "truehd",
    "h264_qsv",
    "hevc_qsv",
    "mpeg2_qsv",
    "vc1_qsv",
    "vp8_qsv",
    "vp9_qsv",
    "av1_qsv",
    "h264_cuvid",
    "hevc_cuvid",
    "mpeg2_cuvid",
    "vc1_cuvid",
    "mpeg4_cuvid",
    "vp8_cuvid",
    "vp9_cuvid",
    "av1_cuvid",
    "h264_rkmpp",
    "hevc_rkmpp",
    "mpeg1_rkmpp",
    "mpeg2_rkmpp",
    "mpeg4_rkmpp",
    "vp8_rkmpp",
    "vp9_rkmpp",
    "av1_rkmpp",
];

/// The encoders Jellyfin enumerates. Port of C# `_requiredEncoders`.
///
/// Same intersection contract as [`REQUIRED_DECODERS`].
pub const REQUIRED_ENCODERS: [&str; 36] = [
    "libx264",
    "libx265",
    "libsvtav1",
    "aac",
    "aac_at",
    "libfdk_aac",
    "ac3",
    "alac",
    "dca",
    "libmp3lame",
    "libopus",
    "libvorbis",
    "flac",
    "truehd",
    "srt",
    "h264_amf",
    "hevc_amf",
    "av1_amf",
    "h264_qsv",
    "hevc_qsv",
    "mjpeg_qsv",
    "av1_qsv",
    "h264_nvenc",
    "hevc_nvenc",
    "av1_nvenc",
    "h264_vaapi",
    "hevc_vaapi",
    "av1_vaapi",
    "mjpeg_vaapi",
    "h264_v4l2m2m",
    "h264_videotoolbox",
    "hevc_videotoolbox",
    "mjpeg_videotoolbox",
    "h264_rkmpp",
    "hevc_rkmpp",
    "mjpeg_rkmpp",
];

/// The filters Jellyfin enumerates. Port of C# `_requiredFilters`, grouped by
/// the backend that uses them.
///
/// Same intersection contract as [`REQUIRED_DECODERS`]. `alphasrc` is the
/// load-bearing entry: without it no vendor's hardware filter chain runs at
/// all, because every chain needs it to render text subtitles for hardware
/// overlay.
pub const REQUIRED_FILTERS: [&str; 41] = [
    // sw
    "alphasrc",
    "zscale",
    "tonemapx",
    // qsv
    "scale_qsv",
    "vpp_qsv",
    "deinterlace_qsv",
    "overlay_qsv",
    // cuda
    "scale_cuda",
    "yadif_cuda",
    "bwdif_cuda",
    "tonemap_cuda",
    "overlay_cuda",
    "transpose_cuda",
    "hwupload_cuda",
    // opencl
    "scale_opencl",
    "tonemap_opencl",
    "overlay_opencl",
    "transpose_opencl",
    "yadif_opencl",
    "bwdif_opencl",
    // vaapi
    "scale_vaapi",
    "deinterlace_vaapi",
    "tonemap_vaapi",
    "procamp_vaapi",
    "overlay_vaapi",
    "transpose_vaapi",
    "hwupload_vaapi",
    // vulkan
    "libplacebo",
    "scale_vulkan",
    "overlay_vulkan",
    "transpose_vulkan",
    "flip_vulkan",
    // videotoolbox
    "yadif_videotoolbox",
    "bwdif_videotoolbox",
    "scale_vt",
    "transpose_vt",
    "overlay_videotoolbox",
    "tonemap_videotoolbox",
    // rkrga
    "scale_rkrga",
    "vpp_rkrga",
    "overlay_rkrga",
];

/// The Vulkan extensions whose presence means the VAAPI device can expose DRM
/// format modifiers. Port of C# `MediaEncoder._vulkanImageDrmFmtModifierExts`.
pub const VULKAN_IMAGE_DRM_FMT_MODIFIER_EXTS: [&str; 1] = ["VK_EXT_image_drm_format_modifier"];

/// The Vulkan extensions whose presence means the VAAPI device can do
/// zero-copy DMA-BUF interop — the gate for the AMD libplacebo filter path.
/// Port of C# `MediaEncoder._vulkanExternalMemoryDmaBufExts`.
pub const VULKAN_EXTERNAL_MEMORY_DMA_BUF_EXTS: [&str; 4] = [
    "VK_KHR_external_memory_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_KHR_external_semaphore_fd",
    "VK_EXT_external_memory_host",
];

/// The `(filter, option)` pair probed for each [`FilterOption`]. Port of C#
/// `_filterOptionsDict`.
#[must_use]
pub const fn filter_option_probe(option: FilterOption) -> (&'static str, &'static str) {
    match option {
        FilterOption::ScaleCudaFormat => ("scale_cuda", "format"),
        FilterOption::TonemapCudaName => ("tonemap_cuda", "GPU accelerated HDR to SDR tonemapping"),
        FilterOption::TonemapOpenclBt2390 => ("tonemap_opencl", "bt2390"),
        FilterOption::OverlayOpenclFrameSync => (
            "overlay_opencl",
            "Action to take when encountering EOF from secondary input",
        ),
        FilterOption::OverlayVaapiFrameSync => (
            "overlay_vaapi",
            "Action to take when encountering EOF from secondary input",
        ),
        FilterOption::OverlayVulkanFrameSync => (
            "overlay_vulkan",
            "Action to take when encountering EOF from secondary input",
        ),
        FilterOption::TransposeOpenclReversal => ("transpose_opencl", "rotate by half-turn"),
        FilterOption::OverlayOpenclAlphaFormat => ("overlay_opencl", "alpha_format"),
        FilterOption::OverlayCudaAlphaFormat => ("overlay_cuda", "alpha_format"),
    }
}

/// The `(bitstream filter, option)` pair probed for each [`BsfOption`]. Port of
/// C# `_bsfOptionsDict`.
#[must_use]
pub const fn bsf_option_probe(option: BsfOption) -> (&'static str, &'static str) {
    match option {
        BsfOption::HevcMetadataRemoveDovi => ("hevc_metadata", "remove_dovi"),
        BsfOption::HevcMetadataRemoveHdr10Plus => ("hevc_metadata", "remove_hdr10plus"),
        BsfOption::Av1MetadataRemoveDovi => ("av1_metadata", "remove_dovi"),
        BsfOption::Av1MetadataRemoveHdr10Plus => ("av1_metadata", "remove_hdr10plus"),
        BsfOption::DoviRpuStrip => ("dovi_rpu", "strip"),
    }
}

/// Validates ffmpeg version output (the pure half of `EncoderValidator`).
///
/// Construct with the encoder path; the version-parsing/validation methods work
/// purely on captured `ffmpeg -version` text without spawning a process.
#[derive(Debug, Clone)]
pub struct EncoderValidator {
    encoder_path: String,
}

impl EncoderValidator {
    /// Creates a validator for the given `ffmpeg` executable path.
    #[must_use]
    pub fn new(encoder_path: impl Into<String>) -> Self {
        Self {
            encoder_path: encoder_path.into(),
        }
    }

    /// The `ffmpeg` executable path this validator was configured with.
    #[must_use]
    pub fn encoder_path(&self) -> &str {
        &self.encoder_path
    }

    /// Validates captured `ffmpeg -version` output against the recommended range.
    ///
    /// Returns `false` for avconv (Libav) output, an unparseable version, or a
    /// version below [`MIN_VERSION`] / above [`MAX_VERSION`]. Mirrors C#
    /// `ValidateVersionInternal`.
    #[must_use]
    pub fn validate_version_internal(&self, version_output: &str) -> bool {
        if version_output
            .to_ascii_lowercase()
            .contains("libav developers")
        {
            // avconv instead of ffmpeg is not supported
            return false;
        }

        // Work out what the version under test is
        let Some(version) = self.get_ffmpeg_version_internal(version_output) else {
            // Version is unknown
            return false;
        };

        if version < MIN_VERSION {
            // Version is below what we recommend
            return false;
        }

        if let Some(max) = MAX_VERSION
            && version > max
        {
            // Version is above what we recommend
            return false;
        }

        true
    }

    /// Works out the ffmpeg version from `ffmpeg -version` output.
    ///
    /// For pre-built binaries the version is at the very start of the output and
    /// is parsed directly. Otherwise the library versions are matched against
    /// [`FFMPEG_MINIMUM_LIBRARY_VERSIONS`]; if every required library is present
    /// and at least its minimum, [`MIN_VERSION`] is returned, else `None`.
    /// Mirrors C# `GetFFmpegVersionInternal`.
    #[must_use]
    pub fn get_ffmpeg_version_internal(&self, output: &str) -> Option<FfmpegVersion> {
        // For pre-built binaries the FFmpeg version should be mentioned at the very start of the output
        if let Some(caps) = FFMPEG_VERSION_REGEX.captures(output)
            && let Some(result) = FfmpegVersion::try_parse(&caps[1])
        {
            return Some(result);
        }

        let version_map = Self::get_ffmpeg_library_versions(output);

        let mut all_versions_validated = true;

        for (library, minimum_version) in FFMPEG_MINIMUM_LIBRARY_VERSIONS.iter() {
            match version_map.get(*library) {
                Some(found_version) if *found_version >= *minimum_version => {}
                _ => all_versions_validated = false,
            }
        }

        if all_versions_validated {
            Some(MIN_VERSION)
        } else {
            None
        }
    }

    /// Parses captured `ffmpeg -filters` output into the available filter names.
    ///
    /// The pure half of C# `GetFFmpegFilters`: the caller shells out for the
    /// output and the parsed names are intersected with [`REQUIRED_FILTERS`],
    /// exactly as upstream does.
    #[must_use]
    pub fn get_filters_internal(output: &str) -> Vec<String> {
        retain_required(
            parse_names(&FILTER_REGEX, output, "filter"),
            &REQUIRED_FILTERS,
        )
    }

    /// Parses captured `ffmpeg -encoders` output into the available encoder
    /// names, intersected with [`REQUIRED_ENCODERS`].
    ///
    /// The pure half of C# `GetCodecs(Codec.Encoder)`.
    #[must_use]
    pub fn get_encoders_internal(output: &str) -> Vec<String> {
        retain_required(
            parse_names(&CODEC_REGEX, output, "codec"),
            &REQUIRED_ENCODERS,
        )
    }

    /// Parses captured `ffmpeg -decoders` output into the available decoder
    /// names, intersected with [`REQUIRED_DECODERS`].
    ///
    /// The pure half of C# `GetCodecs(Codec.Decoder)`.
    #[must_use]
    pub fn get_decoders_internal(output: &str) -> Vec<String> {
        retain_required(
            parse_names(&CODEC_REGEX, output, "codec"),
            &REQUIRED_DECODERS,
        )
    }

    /// Parses captured `ffmpeg -hwaccels` output into the available hardware
    /// acceleration method names.
    ///
    /// The pure half of C# `GetHwaccelTypes`: split on line breaks discarding
    /// empties, drop the first line (the `Hardware acceleration methods:`
    /// header), and de-duplicate. Unlike the codec and filter probes there is
    /// no allowlist — whatever ffmpeg names is taken at face value.
    #[must_use]
    pub fn get_hwaccels_internal(output: &str) -> Vec<String> {
        // C# bails on `IsNullOrWhiteSpace` before splitting. Without this, an
        // all-whitespace output would survive as a whitespace-named "hwaccel",
        // because splitting on line breaks only discards *empty* entries.
        if output.trim().is_empty() {
            return Vec::new();
        }
        let mut seen = Vec::new();
        for line in output
            .split(['\r', '\n'])
            .filter(|line| !line.is_empty())
            .skip(1)
        {
            let name = line.to_owned();
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
        seen
    }

    /// Whether captured `ffmpeg -h filter=<filter>` output shows `filter`
    /// supporting `option`.
    ///
    /// Port of C# `CheckFilterWithOption`. The header line has to name the
    /// filter first: asking about a filter this build does not have prints a
    /// generic help page that could contain the option substring by accident.
    #[must_use]
    pub fn check_filter_with_option_internal(output: &str, filter: &str, option: &str) -> bool {
        Self::check_help_output(output, "Filter ", filter, option)
    }

    /// Whether captured `ffmpeg -h bsf=<filter>` output shows the bitstream
    /// filter `filter` supporting `option`.
    ///
    /// Port of C# `CheckBitStreamFilterWithOption`.
    #[must_use]
    pub fn check_bsf_with_option_internal(output: &str, filter: &str, option: &str) -> bool {
        Self::check_help_output(output, "Bit stream filter ", filter, option)
    }

    /// The shared shape of the two `-h` probes: the help header must name the
    /// thing asked about, and then the option string must appear.
    fn check_help_output(output: &str, prefix: &str, name: &str, option: &str) -> bool {
        if name.is_empty() || option.is_empty() {
            return false;
        }
        if !output.contains(&format!("{prefix}{name}")) {
            return false;
        }
        output.contains(option)
    }

    /// Whether the VAAPI init probe's **stderr** names `driver_name`.
    ///
    /// Port of C# `CheckVaapiDeviceByDriverName`. The caller runs
    /// `ffmpeg -v verbose -hide_banner -init_hw_device vaapi=va:<render node>`
    /// and passes the captured stderr; `driver_name` is one of
    /// `"Mesa Gallium driver"`, `"Intel iHD driver"`, `"Intel i965 driver"`.
    ///
    /// Two guards belong to the caller, matching C#: run this **only on Linux**,
    /// and **only when the configured render-node path is non-empty**. An empty
    /// path would make the command `-init_hw_device vaapi=va:`, which probes
    /// ffmpeg's *default* device and could report a driver for hardware the
    /// operator never configured.
    #[must_use]
    pub fn check_vaapi_driver_internal(stderr: &str, driver_name: &str) -> bool {
        !driver_name.is_empty() && stderr.contains(driver_name)
    }

    /// Whether the DRM/Vulkan init probe's **stderr** names *every* extension
    /// in `extensions`.
    ///
    /// Port of C# `CheckVulkanDrmDeviceByExtensionName`. The caller runs
    /// `ffmpeg -v verbose -hide_banner -init_hw_device drm=dr:<node>
    /// -init_hw_device vulkan=vk@dr` and passes the captured stderr. An empty
    /// extension list is vacuously true, matching the C# loop.
    ///
    /// As with [`Self::check_vaapi_driver_internal`], the caller must gate on
    /// Linux and on a non-empty render-node path.
    #[must_use]
    pub fn check_vulkan_extensions_internal(stderr: &str, extensions: &[&str]) -> bool {
        extensions.iter().all(|ext| stderr.contains(ext))
    }

    /// Grabs the library names and `major.minor` versions from `ffmpeg -version`
    /// output. Mirrors C# `GetFFmpegLibraryVersions`.
    fn get_ffmpeg_library_versions(output: &str) -> HashMap<String, FfmpegVersion> {
        let mut map = HashMap::new();

        for caps in LIBRARY_REGEX.captures_iter(output) {
            let major: i32 = caps["major"].parse().expect("regex guarantees digits");
            let minor: i32 = caps["minor"].parse().expect("regex guarantees digits");
            let version = FfmpegVersion::new(major, minor);
            map.insert(caps["name"].to_owned(), version);
        }

        map
    }
}

/// Pulls the named capture out of every match of `regex` over `output`.
fn parse_names(regex: &Regex, output: &str, group: &str) -> Vec<String> {
    regex
        .captures_iter(output)
        .map(|caps| caps[group].to_owned())
        .collect()
}

/// Keeps only the names present in `required`, preserving ffmpeg's order.
///
/// Port of the `.Where(x => required.Contains(x))` every C# enumeration ends
/// with. The comparison is ordinal (case-sensitive), as upstream's
/// `string[].Contains` is — ffmpeg prints codec and filter names lowercase.
fn retain_required(names: Vec<String>, required: &[&str]) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| required.contains(&name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    use super::super::test_data as d;

    fn validator() -> EncoderValidator {
        EncoderValidator::new("ffmpeg")
    }

    #[rstest]
    #[case(d::FFMPEG_V701_OUTPUT, Some(FfmpegVersion::with_build(7, 0, 1)))]
    #[case(d::FFMPEG_V611_OUTPUT, Some(FfmpegVersion::with_build(6, 1, 1)))]
    #[case(d::FFMPEG_V60_OUTPUT, Some(FfmpegVersion::new(6, 0)))]
    #[case(d::FFMPEG_V512_OUTPUT, Some(FfmpegVersion::with_build(5, 1, 2)))]
    #[case(d::FFMPEG_V44_OUTPUT, Some(FfmpegVersion::new(4, 4)))]
    #[case(d::FFMPEG_V432_OUTPUT, Some(FfmpegVersion::with_build(4, 3, 2)))]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT2, Some(FfmpegVersion::new(4, 4)))]
    #[case(
        d::FFMPEG_GIT_WITHOUT_LIBPOSTPROC_OUTPUT,
        Some(FfmpegVersion::new(4, 4))
    )]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT, None)]
    fn get_ffmpeg_version_test(
        #[case] version_output: &str,
        #[case] version: Option<FfmpegVersion>,
    ) {
        assert_eq!(
            version,
            validator().get_ffmpeg_version_internal(version_output)
        );
    }

    #[rstest]
    #[case(d::FFMPEG_V701_OUTPUT, true)]
    #[case(d::FFMPEG_V611_OUTPUT, true)]
    #[case(d::FFMPEG_V60_OUTPUT, true)]
    #[case(d::FFMPEG_V512_OUTPUT, true)]
    #[case(d::FFMPEG_V44_OUTPUT, true)]
    #[case(d::FFMPEG_V432_OUTPUT, false)]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT2, true)]
    #[case(d::FFMPEG_GIT_WITHOUT_LIBPOSTPROC_OUTPUT, true)]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT, false)]
    fn validate_version_internal_test(#[case] version_output: &str, #[case] valid: bool) {
        assert_eq!(valid, validator().validate_version_internal(version_output));
    }

    #[test]
    fn get_filters_internal_parses_names_and_keeps_only_required() {
        // Shape of real `ffmpeg -filters` output: a header, two-space-indented
        // legend lines, then one-space-indented ` <flags> <name> <io> <desc>`.
        // `abench` and `scale` parse fine but are not filters any decision
        // consults, so the allowlist drops them — the C# intersection.
        let output = "Filters:\n\
             \x20 T.. = Timeline support\n\
             \x20 .S. = Slice threading\n\
             \x20 A = Audio input/output\n\
             \x20... abench            A->A       Benchmark part of a filtergraph.\n\
             \x20T.C scale             V->V       Scale the input video size.\n\
             \x20... tonemapx          V->V       HDR to SDR tonemapping (SIMD).\n\
             \x20..C scale_cuda        V->V       GPU accelerated video resizer.\n\
             \x20... alphasrc          |->V       Provide an alpha channel source.\n";
        let filters = EncoderValidator::get_filters_internal(output);
        assert_eq!(filters, ["tonemapx", "scale_cuda", "alphasrc"]);
        assert!(EncoderValidator::get_filters_internal("").is_empty());
    }

    #[test]
    fn get_encoders_and_decoders_split_on_their_own_allowlists() {
        // Shape of real `ffmpeg -encoders` output: header, legend, `------`
        // divider, then ` <6 flags> <name> <description>` lines.
        let output = "Encoders:\n\
             \x20V..... = Video\n\
             \x20A..... = Audio\n\
             \x20.F.... = Frame-level multithreading\n\
             \x20------\n\
             \x20V....D libx264              libx264 H.264 / AVC (codec h264)\n\
             \x20V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)\n\
             \x20A....D aac                  AAC (Advanced Audio Coding)\n\
             \x20A....D libfdk_aac           Fraunhofer FDK AAC (codec aac)\n\
             \x20V....D h264_cuvid           Nvidia CUVID H264 decoder (codec h264)\n";
        // `h264_cuvid` is a decoder name, so it survives the decoder allowlist
        // and not the encoder one; `libx264`/`h264_nvenc` the other way round.
        assert_eq!(
            EncoderValidator::get_encoders_internal(output),
            ["libx264", "h264_nvenc", "aac", "libfdk_aac"]
        );
        assert_eq!(
            EncoderValidator::get_decoders_internal(output),
            ["aac", "h264_cuvid"]
        );
        assert!(EncoderValidator::get_encoders_internal("").is_empty());
        assert!(EncoderValidator::get_decoders_internal("").is_empty());
    }

    #[test]
    fn get_hwaccels_internal_drops_the_header_and_duplicates() {
        // Real `ffmpeg -hwaccels` output: a header line, then one method per
        // line. Upstream skips exactly one line after dropping empties, so a
        // leading blank line would shift what is treated as the header — that
        // is the upstream behaviour and the test pins it.
        let output = "Hardware acceleration methods:\nvaapi\nqsv\ncuda\nvaapi\n\n";
        assert_eq!(
            EncoderValidator::get_hwaccels_internal(output),
            ["vaapi", "qsv", "cuda"]
        );
        assert!(EncoderValidator::get_hwaccels_internal("").is_empty());
        // Whitespace-only output is nothing, not a blank-named method.
        assert!(EncoderValidator::get_hwaccels_internal(" \n \n \n").is_empty());
        // Header only: nothing available.
        assert!(
            EncoderValidator::get_hwaccels_internal("Hardware acceleration methods:\n").is_empty()
        );
    }

    #[test]
    fn filter_option_probe_requires_the_named_filter_header() {
        let (filter, option) = filter_option_probe(FilterOption::OverlayCudaAlphaFormat);
        assert_eq!((filter, option), ("overlay_cuda", "alpha_format"));

        let help = "Filter overlay_cuda\n  Overlay one video on top of another using CUDA.\n\
                    \x20 alpha_format      <int>  ..FV..... alpha format (default straight)\n";
        assert!(EncoderValidator::check_filter_with_option_internal(
            help, filter, option
        ));

        // Same option text, but the build does not have the filter: ffmpeg
        // prints a generic help page and upstream refuses to infer support.
        let generic = "Usage: ffmpeg [options]\n  alpha_format mentioned in passing\n";
        assert!(!EncoderValidator::check_filter_with_option_internal(
            generic, filter, option
        ));

        // Filter present, option absent (an older build).
        let older = "Filter overlay_cuda\n  Overlay one video on top of another using CUDA.\n";
        assert!(!EncoderValidator::check_filter_with_option_internal(
            older, filter, option
        ));

        // Empty inputs are rejected rather than matching everything.
        assert!(!EncoderValidator::check_filter_with_option_internal(
            help, "", option
        ));
        assert!(!EncoderValidator::check_filter_with_option_internal(
            help, filter, ""
        ));
    }

    #[test]
    fn bsf_option_probe_requires_the_bitstream_filter_header() {
        let (filter, option) = bsf_option_probe(BsfOption::HevcMetadataRemoveDovi);
        assert_eq!((filter, option), ("hevc_metadata", "remove_dovi"));

        let help = "Bit stream filter hevc_metadata\n\
                    \x20 remove_dovi   <boolean>  ....... Remove Dolby Vision metadata\n";
        assert!(EncoderValidator::check_bsf_with_option_internal(
            help, filter, option
        ));
        assert!(!EncoderValidator::check_bsf_with_option_internal(
            "Bit stream filter hevc_metadata\n",
            filter,
            option
        ));
        assert!(!EncoderValidator::check_bsf_with_option_internal(
            "unknown bitstream filter\n",
            filter,
            option
        ));
    }

    #[test]
    fn every_filter_and_bsf_option_probes_an_available_target() {
        // A probe pair naming a filter absent from the allowlist could never
        // report support, because the filter itself would read as unavailable.
        for option in FilterOption::ALL {
            let (filter, opt) = filter_option_probe(option);
            assert!(
                REQUIRED_FILTERS.contains(&filter),
                "{option:?} probes {filter}, which is not in REQUIRED_FILTERS"
            );
            assert!(!opt.is_empty(), "{option:?} has an empty option string");
        }
        for option in BsfOption::ALL {
            let (filter, opt) = bsf_option_probe(option);
            assert!(!filter.is_empty() && !opt.is_empty(), "{option:?} is empty");
        }
    }

    #[test]
    fn vaapi_driver_probe_matches_the_verbose_stderr() {
        // Real shape of the `-init_hw_device vaapi=va:…` verbose stderr.
        let stderr = "[AVHWDeviceContext @ 0x5] libva: VA-API version 1.20.0\n\
                      [AVHWDeviceContext @ 0x5] Initialised VAAPI connection: version 1.20\n\
                      [AVHWDeviceContext @ 0x5] VAAPI driver: Intel iHD driver for Intel(R) Gen Graphics - 24.1.0.\n";
        assert!(EncoderValidator::check_vaapi_driver_internal(
            stderr,
            "Intel iHD driver"
        ));
        assert!(!EncoderValidator::check_vaapi_driver_internal(
            stderr,
            "Intel i965 driver"
        ));
        assert!(!EncoderValidator::check_vaapi_driver_internal(
            stderr,
            "Mesa Gallium driver"
        ));
        assert!(!EncoderValidator::check_vaapi_driver_internal(stderr, ""));
    }

    #[test]
    fn vulkan_extension_probe_requires_every_extension() {
        let all = "Supported extensions:\n\
                   VK_KHR_external_memory_fd\nVK_EXT_external_memory_dma_buf\n\
                   VK_KHR_external_semaphore_fd\nVK_EXT_external_memory_host\n";
        assert!(EncoderValidator::check_vulkan_extensions_internal(
            all,
            &VULKAN_EXTERNAL_MEMORY_DMA_BUF_EXTS
        ));
        assert!(!EncoderValidator::check_vulkan_extensions_internal(
            all,
            &VULKAN_IMAGE_DRM_FMT_MODIFIER_EXTS
        ));

        // One missing extension fails the whole set — interop is all-or-nothing.
        let partial = "VK_KHR_external_memory_fd\nVK_EXT_external_memory_dma_buf\n";
        assert!(!EncoderValidator::check_vulkan_extensions_internal(
            partial,
            &VULKAN_EXTERNAL_MEMORY_DMA_BUF_EXTS
        ));
    }

    #[test]
    fn required_lists_have_no_duplicates() {
        // A duplicate would be a transcription slip against the C# arrays.
        for (name, list) in [
            ("decoders", REQUIRED_DECODERS.as_slice()),
            ("encoders", REQUIRED_ENCODERS.as_slice()),
            ("filters", REQUIRED_FILTERS.as_slice()),
        ] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(before, sorted.len(), "duplicate entry in REQUIRED_{name}");
        }
    }
}
