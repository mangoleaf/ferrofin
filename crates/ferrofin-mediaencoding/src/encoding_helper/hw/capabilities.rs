//! The resolved hardware environment: what the running ffmpeg can do, which OS
//! it runs on, and which kernel is underneath.
//!
//! Port of the capability cache C# spreads across `MediaEncoder` (the
//! `_encoders`/`_decoders`/`_hwaccels`/`_filters` lists, the
//! `_filtersWithOption`/`_bitStreamFiltersWithOption` maps, the
//! `_isVaapiDevice*` probes, `_isVideoToolboxAv1DecodeAvailable`,
//! `_isLowPriorityHwDecodeSupported`, `_ffmpegVersion`) plus the two ambient
//! facts `EncodingHelper` reads straight off the platform
//! (`OperatingSystem.Is{Windows,Linux,MacOS}()` and
//! `Environment.OSVersion.Version`).
//!
//! **The OS is data, not `cfg!`.** Every C# `OperatingSystem.IsWindows()` test
//! becomes a [`Platform`] comparison carried in this struct. That is what makes
//! the Windows and macOS branches of the hardware matrix testable from a Linux
//! CI runner — Ferrofin has no Windows machine to test on, so a compile-time
//! gate would mean those branches were never exercised at all.

use crate::encoder::FfmpegVersion;
use crate::encoding_helper::transcode_state::EncoderCapabilities;

/// The operating system the server is running on, as the hardware matrix sees it.
///
/// Port of the `OperatingSystem.Is*()` calls scattered through
/// `EncodingHelper`. Anything that is not Linux, Windows, or macOS resolves to
/// [`Platform::Other`], which every hardware branch rejects — the same outcome
/// C# reaches when all three tests return `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Platform {
    /// Linux — VAAPI, QSV, NVENC, RKMPP, V4L2M2M.
    Linux,
    /// Windows — AMF, QSV (via D3D11), NVENC.
    Windows,
    /// macOS — VideoToolbox.
    MacOs,
    /// Anything else: no hardware acceleration is offered.
    ///
    /// This is the [`Default`], because a default-constructed
    /// [`FfmpegCapabilities`] means "nothing was probed" — the composition root
    /// sets the real platform, and a test that forgets to must fail loudly
    /// rather than silently exercise whichever branch the host happens to
    /// match. It is also the state C# reaches when all three
    /// `OperatingSystem.Is*()` calls return false.
    #[default]
    Other,
}

impl Platform {
    /// The platform this binary is running on, from [`std::env::consts::OS`].
    #[must_use]
    pub fn current() -> Self {
        match std::env::consts::OS {
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            "macos" => Self::MacOs,
            _ => Self::Other,
        }
    }

    /// Port of `OperatingSystem.IsLinux()`.
    #[must_use]
    pub fn is_linux(self) -> bool {
        self == Self::Linux
    }

    /// Port of `OperatingSystem.IsWindows()`.
    #[must_use]
    pub fn is_windows(self) -> bool {
        self == Self::Windows
    }

    /// Port of `OperatingSystem.IsMacOS()`.
    #[must_use]
    pub fn is_macos(self) -> bool {
        self == Self::MacOs
    }
}

/// A filter whose *option* has to be probed, not just its presence.
///
/// Port of C# `FilterOptionType`. Some ffmpeg builds ship a filter without the
/// option Jellyfin needs (an older `overlay_opencl` with no `alpha_format`, a
/// `tonemap_opencl` predating `bt2390`), so presence in `-filters` is not
/// enough — the option string has to be found in `ffmpeg -h filter=<name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterOption {
    /// `scale_cuda` accepts `format`.
    ScaleCudaFormat,
    /// `tonemap_cuda` is the real GPU tonemap (not a stub of the same name).
    TonemapCudaName,
    /// `tonemap_opencl` accepts the `bt2390` algorithm.
    TonemapOpenclBt2390,
    /// `overlay_opencl` supports frame-sync (`eof_action`).
    OverlayOpenclFrameSync,
    /// `overlay_vaapi` supports frame-sync (`eof_action`).
    OverlayVaapiFrameSync,
    /// `overlay_vulkan` supports frame-sync (`eof_action`).
    OverlayVulkanFrameSync,
    /// `transpose_opencl` supports half-turn rotation.
    TransposeOpenclReversal,
    /// `overlay_opencl` accepts `alpha_format`.
    OverlayOpenclAlphaFormat,
    /// `overlay_cuda` accepts `alpha_format`.
    OverlayCudaAlphaFormat,
}

impl FilterOption {
    /// Every variant, in declaration order — the probe worklist.
    pub const ALL: [Self; 9] = [
        Self::ScaleCudaFormat,
        Self::TonemapCudaName,
        Self::TonemapOpenclBt2390,
        Self::OverlayOpenclFrameSync,
        Self::OverlayVaapiFrameSync,
        Self::OverlayVulkanFrameSync,
        Self::TransposeOpenclReversal,
        Self::OverlayOpenclAlphaFormat,
        Self::OverlayCudaAlphaFormat,
    ];

    /// This variant's slot in the [`FfmpegCapabilities`] result array.
    const fn index(self) -> usize {
        match self {
            Self::ScaleCudaFormat => 0,
            Self::TonemapCudaName => 1,
            Self::TonemapOpenclBt2390 => 2,
            Self::OverlayOpenclFrameSync => 3,
            Self::OverlayVaapiFrameSync => 4,
            Self::OverlayVulkanFrameSync => 5,
            Self::TransposeOpenclReversal => 6,
            Self::OverlayOpenclAlphaFormat => 7,
            Self::OverlayCudaAlphaFormat => 8,
        }
    }
}

/// A bitstream filter whose *option* has to be probed.
///
/// Port of C# `BitStreamFilterOptionType`. These gate whether a stream copy can
/// strip Dolby Vision / HDR10+ dynamic metadata for a client that cannot handle
/// it — without them the only alternative is a full re-encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BsfOption {
    /// `hevc_metadata` accepts `remove_dovi`.
    HevcMetadataRemoveDovi,
    /// `hevc_metadata` accepts `remove_hdr10plus`.
    HevcMetadataRemoveHdr10Plus,
    /// `av1_metadata` accepts `remove_dovi`.
    Av1MetadataRemoveDovi,
    /// `av1_metadata` accepts `remove_hdr10plus`.
    Av1MetadataRemoveHdr10Plus,
    /// `dovi_rpu` accepts `strip`.
    DoviRpuStrip,
}

impl BsfOption {
    /// Every variant, in declaration order — the probe worklist.
    pub const ALL: [Self; 5] = [
        Self::HevcMetadataRemoveDovi,
        Self::HevcMetadataRemoveHdr10Plus,
        Self::Av1MetadataRemoveDovi,
        Self::Av1MetadataRemoveHdr10Plus,
        Self::DoviRpuStrip,
    ];

    /// This variant's slot in the [`FfmpegCapabilities`] result array.
    const fn index(self) -> usize {
        match self {
            Self::HevcMetadataRemoveDovi => 0,
            Self::HevcMetadataRemoveHdr10Plus => 1,
            Self::Av1MetadataRemoveDovi => 2,
            Self::Av1MetadataRemoveHdr10Plus => 3,
            Self::DoviRpuStrip => 4,
        }
    }
}

/// Everything the hardware matrix needs to know about the running environment.
///
/// Built once at startup by the composition root (which owns the ffmpeg
/// spawns) and then read-only, so every argument builder downstream is a pure
/// function of `(request, options, capabilities)` and unit-testable with
/// [`FfmpegCapabilities::builder`].
///
/// The `is_vaapi_device_*` flags are deliberately independent booleans rather
/// than one "driver" enum: C# probes them with three separate ffmpeg runs, and
/// a device that matches none of them (an unknown driver) is a real state the
/// matrix handles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "port of C# MediaEncoder's independent probe flags; each is a separate \
              ffmpeg probe with no meaningful grouping"
)]
pub struct FfmpegCapabilities {
    encoders: Vec<String>,
    decoders: Vec<String>,
    hwaccels: Vec<String>,
    filters: Vec<String>,
    filter_options: [bool; FilterOption::ALL.len()],
    bsf_options: [bool; BsfOption::ALL.len()],
    ffmpeg_version: Option<FfmpegVersion>,
    os_version: Option<FfmpegVersion>,
    platform: Platform,
    is_vaapi_device_amd: bool,
    is_vaapi_device_intel_ihd: bool,
    is_vaapi_device_intel_i965: bool,
    vaapi_vulkan_drm_modifier: bool,
    vaapi_vulkan_drm_interop: bool,
    is_videotoolbox_av1_decode: bool,
    low_priority_hwaccel_flag: bool,
}

impl FfmpegCapabilities {
    /// Starts building a capability set. Every field defaults to "absent".
    #[must_use]
    pub fn builder() -> FfmpegCapabilitiesBuilder {
        FfmpegCapabilitiesBuilder {
            caps: Self::default(),
        }
    }

    /// Port of `IMediaEncoder.SupportsDecoder` (case-insensitive).
    #[must_use]
    pub fn supports_decoder(&self, decoder: &str) -> bool {
        contains_ignore_case(&self.decoders, decoder)
    }

    /// Port of `IMediaEncoder.SupportsHwaccel` (case-insensitive).
    #[must_use]
    pub fn supports_hwaccel(&self, hwaccel: &str) -> bool {
        contains_ignore_case(&self.hwaccels, hwaccel)
    }

    /// Port of `IMediaEncoder.SupportsFilter` (case-insensitive).
    #[must_use]
    pub fn supports_filter(&self, filter: &str) -> bool {
        contains_ignore_case(&self.filters, filter)
    }

    /// Port of `IMediaEncoder.SupportsFilterWithOption`.
    #[must_use]
    pub fn supports_filter_with_option(&self, option: FilterOption) -> bool {
        // Looked up rather than subscripted so a variant added to `index()` but
        // forgotten in `ALL` degrades to "unsupported" instead of panicking
        // inside a getter. The compiler forces `index()` to stay exhaustive; it
        // cannot force `ALL` to stay complete.
        self.filter_options
            .get(option.index())
            .copied()
            .unwrap_or(false)
    }

    /// Port of `IMediaEncoder.SupportsBitStreamFilterWithOption`.
    #[must_use]
    pub fn supports_bsf_with_option(&self, option: BsfOption) -> bool {
        self.bsf_options
            .get(option.index())
            .copied()
            .unwrap_or(false)
    }

    /// The detected ffmpeg version, or `None` when the probe could not
    /// determine one. Port of `IMediaEncoder.EncoderVersion`.
    ///
    /// A `None` version fails every [`Self::ffmpeg_at_least`] gate, which is the
    /// conservative direction: an unidentifiable build is assumed to lack the
    /// newer options.
    #[must_use]
    pub fn ffmpeg_version(&self) -> Option<FfmpegVersion> {
        self.ffmpeg_version
    }

    /// Whether the detected ffmpeg is at least `min` — the shape every
    /// `_minFFmpeg*` gate in C# is used in.
    #[must_use]
    pub fn ffmpeg_at_least(&self, min: FfmpegVersion) -> bool {
        self.ffmpeg_version.is_some_and(|v| v >= min)
    }

    /// The operating system's version, or `None` when it could not be read.
    ///
    /// Port of `Environment.OSVersion.Version`, which means **two different
    /// things** depending on platform, exactly as it does in C#: the *kernel*
    /// version on Linux (what the i915 hang workaround and the AMD
    /// VAAPI/Vulkan interop gate test) and the *macOS release* on macOS (what
    /// the VideoToolbox H.264 Hi10P gate tests against 14.6).
    ///
    /// `None` means "not read" and fails every gate.
    ///
    /// **On Linux that is exactly upstream's behaviour**, not merely the
    /// conservative direction: when .NET cannot parse a release it yields
    /// `0.0.0.0`, which is below `_minKerneli915Hang` (so: no workaround) and
    /// below `_minKernelVersionAmdVkFmtModifier` (so: no Vulkan interop) — the
    /// same two outcomes `None` produces.
    ///
    /// **On macOS it is a real divergence.** .NET does not parse a release
    /// string there at all; `Environment.OSVersion.OSX.cs` asks Objective-C for
    /// the product version. Ferrofin reports `None` on macOS today, so the
    /// VideoToolbox H.264 Hi10P gate (`>= 14.6`) never opens and Hi10P falls
    /// back to software decoding. Closing that is a named work item of
    /// `brain/plans/PLAN_HWACCEL.md` phase 6 (VideoToolbox).
    #[must_use]
    pub fn os_version(&self) -> Option<FfmpegVersion> {
        self.os_version
    }

    /// The operating system the hardware branches switch on.
    #[must_use]
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// Whether the configured VAAPI render node is an AMD (Mesa Gallium)
    /// device. Port of `IMediaEncoder.IsVaapiDeviceAmd`.
    #[must_use]
    pub fn is_vaapi_device_amd(&self) -> bool {
        self.is_vaapi_device_amd
    }

    /// Whether the configured VAAPI render node uses the Intel iHD driver.
    /// Port of `IMediaEncoder.IsVaapiDeviceInteliHD`.
    #[must_use]
    pub fn is_vaapi_device_intel_ihd(&self) -> bool {
        self.is_vaapi_device_intel_ihd
    }

    /// Whether the configured VAAPI render node uses the legacy Intel i965
    /// driver. Port of `IMediaEncoder.IsVaapiDeviceInteli965`.
    #[must_use]
    pub fn is_vaapi_device_intel_i965(&self) -> bool {
        self.is_vaapi_device_intel_i965
    }

    /// Whether the VAAPI device's Vulkan driver exposes the DRM format-modifier
    /// extension. Port of `IMediaEncoder.IsVaapiDeviceSupportVulkanDrmModifier`.
    #[must_use]
    pub fn vaapi_vulkan_drm_modifier(&self) -> bool {
        self.vaapi_vulkan_drm_modifier
    }

    /// Whether the VAAPI device's Vulkan driver exposes the external-memory
    /// DMA-BUF interop extensions — the gate for the AMD libplacebo path. Port
    /// of `IMediaEncoder.IsVaapiDeviceSupportVulkanDrmInterop`.
    #[must_use]
    pub fn vaapi_vulkan_drm_interop(&self) -> bool {
        self.vaapi_vulkan_drm_interop
    }

    /// Whether this Mac can decode AV1 in hardware. Port of
    /// `IMediaEncoder.IsVideoToolboxAv1DecodeAvailable`.
    #[must_use]
    pub fn is_videotoolbox_av1_decode(&self) -> bool {
        self.is_videotoolbox_av1_decode
    }

    /// Whether ffmpeg accepts `-hwaccel_flags +low_priority`, used by the
    /// accelerated trickplay extraction. Port of
    /// `MediaEncoder._isLowPriorityHwDecodeSupported`.
    #[must_use]
    pub fn low_priority_hwaccel_flag(&self) -> bool {
        self.low_priority_hwaccel_flag
    }
}

/// Port of `IMediaEncoder.SupportsEncoder`, kept on the pre-existing seam so the
/// software audio-encoder selection is untouched by the hardware port.
impl EncoderCapabilities for FfmpegCapabilities {
    fn supports_encoder(&self, encoder: &str) -> bool {
        contains_ignore_case(&self.encoders, encoder)
    }
}

/// `StringComparer.OrdinalIgnoreCase` membership, which is how every C#
/// `Supports*` list lookup is written.
fn contains_ignore_case(haystack: &[String], needle: &str) -> bool {
    haystack
        .iter()
        .any(|item| item.eq_ignore_ascii_case(needle))
}

/// Fluent builder for [`FfmpegCapabilities`].
///
/// The composition root fills it from the startup ffmpeg probes; tests fill it
/// with exactly the capabilities the case under test needs, which is how the
/// per-vendor argument builders are exercised on a machine that has none of the
/// hardware.
#[derive(Debug, Clone)]
pub struct FfmpegCapabilitiesBuilder {
    caps: FfmpegCapabilities,
}

impl FfmpegCapabilitiesBuilder {
    /// Sets the available encoder names.
    #[must_use]
    pub fn encoders<I, S>(mut self, encoders: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.caps.encoders = encoders.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the available decoder names.
    #[must_use]
    pub fn decoders<I, S>(mut self, decoders: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.caps.decoders = decoders.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the available `-hwaccels` names.
    #[must_use]
    pub fn hwaccels<I, S>(mut self, hwaccels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.caps.hwaccels = hwaccels.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the available filter names.
    #[must_use]
    pub fn filters<I, S>(mut self, filters: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.caps.filters = filters.into_iter().map(Into::into).collect();
        self
    }

    /// Records the result of one filter-option probe.
    #[must_use]
    pub fn filter_option(mut self, option: FilterOption, supported: bool) -> Self {
        self.caps.filter_options[option.index()] = supported;
        self
    }

    /// Records the result of one bitstream-filter-option probe.
    #[must_use]
    pub fn bsf_option(mut self, option: BsfOption, supported: bool) -> Self {
        self.caps.bsf_options[option.index()] = supported;
        self
    }

    /// Sets every filter option to `supported` — the shorthand a test uses when
    /// the options are not what it is exercising.
    #[must_use]
    pub fn all_filter_options(mut self, supported: bool) -> Self {
        self.caps.filter_options = [supported; FilterOption::ALL.len()];
        self
    }

    /// Sets every bitstream-filter option to `supported`.
    #[must_use]
    pub fn all_bsf_options(mut self, supported: bool) -> Self {
        self.caps.bsf_options = [supported; BsfOption::ALL.len()];
        self
    }

    /// Sets the detected ffmpeg version.
    #[must_use]
    pub fn ffmpeg_version(mut self, version: FfmpegVersion) -> Self {
        self.caps.ffmpeg_version = Some(version);
        self
    }

    /// Sets the operating system's version — the kernel version on Linux, the
    /// macOS release on macOS. See [`FfmpegCapabilities::os_version`].
    #[must_use]
    pub fn os_version(mut self, version: FfmpegVersion) -> Self {
        self.caps.os_version = Some(version);
        self
    }

    /// Sets the operating system the hardware branches switch on.
    #[must_use]
    pub fn platform(mut self, platform: Platform) -> Self {
        self.caps.platform = platform;
        self
    }

    /// Sets the three VAAPI driver-detection flags at once.
    #[must_use]
    pub fn vaapi_driver(mut self, amd: bool, intel_ihd: bool, intel_i965: bool) -> Self {
        self.caps.is_vaapi_device_amd = amd;
        self.caps.is_vaapi_device_intel_ihd = intel_ihd;
        self.caps.is_vaapi_device_intel_i965 = intel_i965;
        self
    }

    /// Sets the two VAAPI/Vulkan interop probe results.
    #[must_use]
    pub fn vaapi_vulkan(mut self, drm_modifier: bool, drm_interop: bool) -> Self {
        self.caps.vaapi_vulkan_drm_modifier = drm_modifier;
        self.caps.vaapi_vulkan_drm_interop = drm_interop;
        self
    }

    /// Sets whether this Mac can decode AV1 in hardware.
    #[must_use]
    pub fn videotoolbox_av1_decode(mut self, available: bool) -> Self {
        self.caps.is_videotoolbox_av1_decode = available;
        self
    }

    /// Sets whether `-hwaccel_flags +low_priority` is accepted.
    #[must_use]
    pub fn low_priority_hwaccel_flag(mut self, supported: bool) -> Self {
        self.caps.low_priority_hwaccel_flag = supported;
        self
    }

    /// Finishes the capability set.
    #[must_use]
    pub fn build(self) -> FfmpegCapabilities {
        self.caps
    }
}

/// Parses a `uname -r` style release string the way .NET does.
///
/// Port of `Environment.OSVersion`'s Unix implementation: take the **first four
/// digit runs anywhere in the string**, ignoring every separator, and build a
/// fully specified four-component version with `0` for any run that is not
/// there. Nothing is rejected — a string with no digits at all yields `0.0.0.0`,
/// exactly as .NET produces.
///
/// Getting this exactly right matters because the i915 workaround tests a
/// *closed* range whose upper bound is the three-component `6.1.3`
/// ([`MAX_KERNEL_I915_HANG`](super::versions::MAX_KERNEL_I915_HANG)). A release
/// of `6.1.3-arch1-1` parses to `6.1.3.1`, and under .NET's `System.Version`
/// ordering — which [`FfmpegVersion`] reproduces, unspecified components being
/// `-1` — that sorts *above* `6.1.3` and so escapes the workaround. Truncating
/// to `6.1.3` instead would sort *equal* and wrongly apply it.
///
/// Overflow is clamped to [`i32::MAX`] and abandons the rest of that digit run,
/// matching the `checked`/`OverflowException` handling upstream added for
/// releases like `4.15.0-24201807041620-generic`.
#[must_use]
pub fn parse_os_release(release: &str) -> FfmpegVersion {
    let bytes = release.as_bytes();
    let mut pos = 0;
    let mut parts = [0_i32; 4];
    for part in &mut parts {
        *part = next_number(bytes, &mut pos);
    }
    FfmpegVersion::with_revision(parts[0], parts[1], parts[2], parts[3])
}

/// Skips to the next digit run and parses it, or returns `0` at end of input.
/// Port of .NET `Environment.FindAndParseNextNumber`.
fn next_number(bytes: &[u8], pos: &mut usize) -> i32 {
    while *pos < bytes.len() && !bytes[*pos].is_ascii_digit() {
        *pos += 1;
    }
    let mut num: i32 = 0;
    while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
        let digit = i32::from(bytes[*pos] - b'0');
        let Some(next) = num.checked_mul(10).and_then(|n| n.checked_add(digit)) else {
            // Upstream clamps and stops scanning this run, leaving the cursor
            // inside it so the next call resumes mid-run. Reproduced exactly.
            return i32::MAX;
        };
        num = next;
        *pos += 1;
    }
    num
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn platform_predicates_are_mutually_exclusive() {
        assert!(Platform::Linux.is_linux());
        assert!(!Platform::Linux.is_windows());
        assert!(!Platform::Linux.is_macos());
        assert!(Platform::Windows.is_windows());
        assert!(!Platform::Windows.is_linux());
        assert!(Platform::MacOs.is_macos());
        assert!(!Platform::MacOs.is_windows());
        // An unrecognised OS answers "no" to all three, exactly as the C#
        // triple of `OperatingSystem.Is*()` calls would.
        assert!(!Platform::Other.is_linux());
        assert!(!Platform::Other.is_windows());
        assert!(!Platform::Other.is_macos());
    }

    #[test]
    fn platform_current_matches_the_build_target() {
        // Whatever this test runs on, `current()` must agree with `cfg!` — the
        // one place the two views of the OS are allowed to be compared.
        let expected = if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Other
        };
        assert_eq!(Platform::current(), expected);
    }

    #[test]
    fn lookups_are_case_insensitive_like_the_csharp_comparer() {
        let caps = FfmpegCapabilities::builder()
            .encoders(["h264_nvenc"])
            .decoders(["hevc_qsv"])
            .hwaccels(["vaapi"])
            .filters(["scale_cuda"])
            .build();
        assert!(caps.supports_encoder("H264_NVENC"));
        assert!(caps.supports_decoder("HEVC_QSV"));
        assert!(caps.supports_hwaccel("VAAPI"));
        assert!(caps.supports_filter("SCALE_CUDA"));
        assert!(!caps.supports_encoder("h264_qsv"));
        assert!(!caps.supports_decoder("h264_qsv"));
        assert!(!caps.supports_hwaccel("qsv"));
        assert!(!caps.supports_filter("scale_vaapi"));
    }

    #[test]
    fn empty_capabilities_support_nothing() {
        let caps = FfmpegCapabilities::default();
        assert!(!caps.supports_encoder("h264_nvenc"));
        assert!(!caps.supports_decoder("h264_cuvid"));
        assert!(!caps.supports_hwaccel("cuda"));
        assert!(!caps.supports_filter("alphasrc"));
        assert_eq!(caps.ffmpeg_version(), None);
        assert_eq!(caps.os_version(), None);
        assert_eq!(caps.platform(), Platform::Other);
        assert!(!caps.is_vaapi_device_amd());
        assert!(!caps.is_vaapi_device_intel_ihd());
        assert!(!caps.is_vaapi_device_intel_i965());
        assert!(!caps.vaapi_vulkan_drm_modifier());
        assert!(!caps.vaapi_vulkan_drm_interop());
        assert!(!caps.is_videotoolbox_av1_decode());
        assert!(!caps.low_priority_hwaccel_flag());
        // An unprobed platform answers "no" to every OS test, so no hardware
        // branch can be entered by accident.
        assert!(!caps.platform().is_linux());
        assert!(!caps.platform().is_windows());
        assert!(!caps.platform().is_macos());
        for option in FilterOption::ALL {
            assert!(!caps.supports_filter_with_option(option));
        }
        for option in BsfOption::ALL {
            assert!(!caps.supports_bsf_with_option(option));
        }
    }

    #[test]
    fn each_filter_option_has_its_own_slot() {
        // A per-variant slot mix-up would make one probe answer for another, so
        // set exactly one and require the other eight to stay false.
        for option in FilterOption::ALL {
            let caps = FfmpegCapabilities::builder()
                .filter_option(option, true)
                .build();
            for other in FilterOption::ALL {
                assert_eq!(
                    caps.supports_filter_with_option(other),
                    other == option,
                    "{option:?} set, {other:?} read"
                );
            }
        }
    }

    #[test]
    fn each_bsf_option_has_its_own_slot() {
        for option in BsfOption::ALL {
            let caps = FfmpegCapabilities::builder()
                .bsf_option(option, true)
                .build();
            for other in BsfOption::ALL {
                assert_eq!(
                    caps.supports_bsf_with_option(other),
                    other == option,
                    "{option:?} set, {other:?} read"
                );
            }
        }
    }

    #[test]
    fn bulk_option_setters_cover_every_variant() {
        let caps = FfmpegCapabilities::builder()
            .all_filter_options(true)
            .all_bsf_options(true)
            .build();
        assert!(
            FilterOption::ALL
                .into_iter()
                .all(|o| caps.supports_filter_with_option(o))
        );
        assert!(
            BsfOption::ALL
                .into_iter()
                .all(|o| caps.supports_bsf_with_option(o))
        );
    }

    #[test]
    fn version_gates_read_the_probed_version() {
        let caps = FfmpegCapabilities::builder()
            .ffmpeg_version(FfmpegVersion::with_build(7, 0, 1))
            .os_version(FfmpegVersion::with_build(6, 1, 3))
            .platform(Platform::Windows)
            .build();
        assert_eq!(
            caps.ffmpeg_version(),
            Some(FfmpegVersion::with_build(7, 0, 1))
        );
        assert_eq!(caps.os_version(), Some(FfmpegVersion::with_build(6, 1, 3)));
        assert_eq!(caps.platform(), Platform::Windows);
        assert!(caps.ffmpeg_at_least(FfmpegVersion::new(6, 0)));
        assert!(caps.ffmpeg_at_least(FfmpegVersion::with_build(7, 0, 1)));
        assert!(!caps.ffmpeg_at_least(FfmpegVersion::with_build(7, 1, 1)));
    }

    #[test]
    fn an_unknown_ffmpeg_version_fails_every_gate() {
        // The conservative direction: a build we cannot identify is assumed to
        // lack the newer options rather than assumed to have them.
        let caps = FfmpegCapabilities::default();
        assert!(!caps.ffmpeg_at_least(FfmpegVersion::new(4, 4)));
    }

    #[test]
    fn probe_flags_round_trip() {
        let caps = FfmpegCapabilities::builder()
            .vaapi_driver(false, true, false)
            .vaapi_vulkan(true, false)
            .videotoolbox_av1_decode(true)
            .low_priority_hwaccel_flag(true)
            .build();
        assert!(!caps.is_vaapi_device_amd());
        assert!(caps.is_vaapi_device_intel_ihd());
        assert!(!caps.is_vaapi_device_intel_i965());
        assert!(caps.vaapi_vulkan_drm_modifier());
        assert!(!caps.vaapi_vulkan_drm_interop());
        assert!(caps.is_videotoolbox_av1_decode());
        assert!(caps.low_priority_hwaccel_flag());
    }

    #[rstest]
    // The distribution suffix is not dropped — its digits ARE the remaining
    // components, because .NET scans for digit runs and ignores separators.
    #[case("6.1.3-arch1-1", FfmpegVersion::with_revision(6, 1, 3, 1))]
    #[case("5.15.0-107-generic", FfmpegVersion::with_revision(5, 15, 0, 107))]
    #[case("6.12.1-rc1+", FfmpegVersion::with_revision(6, 12, 1, 1))]
    // Missing runs are 0, never "unspecified".
    #[case("6.0.18", FfmpegVersion::with_revision(6, 0, 18, 0))]
    #[case("5.18", FfmpegVersion::with_revision(5, 18, 0, 0))]
    #[case("6.1.", FfmpegVersion::with_revision(6, 1, 0, 0))]
    #[case("6", FfmpegVersion::with_revision(6, 0, 0, 0))]
    // Nothing numeric at all still yields a version, exactly as .NET does.
    #[case("", FfmpegVersion::with_revision(0, 0, 0, 0))]
    #[case("unknown", FfmpegVersion::with_revision(0, 0, 0, 0))]
    // The overflow release upstream names in its own comment.
    #[case(
        "4.15.0-24201807041620-generic",
        FfmpegVersion::with_revision(4, 15, 0, i32::MAX)
    )]
    // Overflow in a NON-final run, which is the only way to observe that the
    // cursor is left ON the offending digit rather than past it: the eleven
    // nines overflow at the tenth, so the next scan resumes there and reads the
    // last two as `99`, and only then does `.1` become the third component.
    #[case("99999999999.1", FfmpegVersion::with_revision(i32::MAX, 99, 1, 0))]
    fn os_release_parsing(#[case] release: &str, #[case] expected: FfmpegVersion) {
        assert_eq!(parse_os_release(release), expected);
    }

    #[test]
    fn a_point_release_kernel_sorts_above_the_i915_range_bound() {
        // The boundary this parser exists for. `_maxKerneli915Hang` is the
        // three-component 6.1.3; a real 6.1.3 kernel release always carries a
        // fourth number, so it sorts ABOVE the bound and escapes the
        // workaround. Truncating the suffix would compare equal and wrongly
        // apply it.
        let bound = super::super::versions::MAX_KERNEL_I915_HANG;
        assert!(parse_os_release("6.1.3-arch1-1") > bound);
        assert!(parse_os_release("6.1.2-arch1-1") < bound);
        // The lower bound behaves the same way round.
        let min = super::super::versions::MIN_KERNEL_I915_HANG;
        assert!(parse_os_release("5.18.0-1-amd64") > min);
        assert!(parse_os_release("5.17.9") < min);
    }
}
