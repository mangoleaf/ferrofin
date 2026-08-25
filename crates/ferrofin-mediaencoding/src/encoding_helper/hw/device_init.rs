//! The `-init_hw_device` graph builders.
//!
//! Port of C# `EncodingHelper`'s device-argument methods (10.11.z lines
//! 809–961). Before ffmpeg can run a hardware filter it needs a *device*, and
//! for several backends it needs a small graph of them: QSV on Linux is derived
//! from a VAAPI device, QSV on Windows from a D3D11 device, and the AMD
//! libplacebo path chains DRM → VAAPI → Vulkan so the three can share buffers.
//! Each device gets a short alias that later arguments refer to.
//!
//! Every function here returns a string that **begins with a space** and is
//! meant to be concatenated; the caller trims once at the end. That is exactly
//! how the C# builds it, and keeping the shape means a golden string can be
//! compared against upstream output by eye.

use crate::encoder::FfmpegVersion;

use super::capabilities::Platform;
use super::versions::MIN_FFMPEG_VAAPI_DEVICE_VENDOR_ID;

/// Intel Quick Sync device alias. Port of C# `QsvAlias`.
pub const QSV_ALIAS: &str = "qs";
/// VAAPI device alias. Port of C# `VaapiAlias`.
pub const VAAPI_ALIAS: &str = "va";
/// Direct3D 11 video acceleration device alias. Port of C# `D3d11vaAlias`.
pub const D3D11VA_ALIAS: &str = "dx11";
/// VideoToolbox device alias. Port of C# `VideotoolboxAlias`.
pub const VIDEOTOOLBOX_ALIAS: &str = "vt";
/// Rockchip MPP device alias. Port of C# `RkmppAlias`.
pub const RKMPP_ALIAS: &str = "rk";
/// OpenCL device alias. Port of C# `OpenclAlias`.
pub const OPENCL_ALIAS: &str = "ocl";
/// CUDA device alias. Port of C# `CudaAlias`.
pub const CUDA_ALIAS: &str = "cu";
/// DRM device alias. Port of C# `DrmAlias`.
pub const DRM_ALIAS: &str = "dr";
/// Vulkan device alias. Port of C# `VulkanAlias`.
pub const VULKAN_ALIAS: &str = "vk";

/// The render node used when none is configured. Same value as the fallback in
/// C# `GetDrmDeviceArgs`; see that function for the one behavioural difference.
pub const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// The Intel PCI vendor id, used to pin a D3D11 or VAAPI adapter. Port of the
/// literal `"0x8086"` in `GetQsvDeviceArgs`.
pub const VENDOR_ID_INTEL: &str = "0x8086";

/// The AMD PCI vendor id, used to pin the D3D11 adapter for AMF. Port of the
/// literal `"0x1002"` in `GetInputVideoHwaccelArgs`.
pub const VENDOR_ID_AMD: &str = "0x1002";

/// The OpenCL device-vendor string that selects AMD's ROCm/ROCr runtime. Port
/// of the literal in the AMD VAAPI branch of `GetInputVideoHwaccelArgs`.
pub const OPENCL_VENDOR_AMD: &str = "Advanced Micro Devices";

/// ` -init_hw_device rkmpp=<alias>`. Port of `GetRkmppDeviceArgs`.
///
/// Rockchip exposes no device selection, so there is nothing to configure.
#[must_use]
pub fn rkmpp_device_args(alias: &str) -> String {
    format!(" -init_hw_device rkmpp={alias}")
}

/// ` -init_hw_device videotoolbox=<alias>`. Port of `GetVideoToolboxDeviceArgs`.
///
/// VideoToolbox exposes no device selection either.
#[must_use]
pub fn videotoolbox_device_args(alias: &str) -> String {
    format!(" -init_hw_device videotoolbox={alias}")
}

/// ` -init_hw_device cuda=<alias>:<index>`. Port of `GetCudaDeviceArgs`.
///
/// A negative index means "unset" and becomes `0`, as in C#.
#[must_use]
pub fn cuda_device_args(device_index: i32, alias: &str) -> String {
    let device_index = device_index.max(0);
    format!(" -init_hw_device cuda={alias}:{device_index}")
}

/// ` -init_hw_device vulkan=…`. Port of `GetVulkanDeviceArgs`.
///
/// Three shapes, in the order C# decides them: derived from another device
/// (`vulkan=vk@dr`), selected by name (`vulkan=vk:"…"`), or by index
/// (`vulkan=vk:0`). A named device wins over the index; a source alias wins
/// over both.
#[must_use]
pub fn vulkan_device_args(
    device_index: i32,
    device_name: Option<&str>,
    src_device_alias: Option<&str>,
    alias: &str,
) -> String {
    let device_index = device_index.max(0);
    let options = match src_device_alias.filter(|s| !s.is_empty()) {
        Some(src) => format!("@{src}"),
        None => match device_name.filter(|s| !s.is_empty()) {
            Some(name) => format!(":\"{name}\""),
            None => format!(":{device_index}"),
        },
    };
    format!(" -init_hw_device vulkan={alias}{options}")
}

/// ` -init_hw_device opencl=…`. Port of `GetOpenclDeviceArgs`.
///
/// Same three shapes as [`vulkan_device_args`], but the default platform/device
/// pair is spelled `:0.0` and a vendor selection is `:.<index>,device_vendor="…"`
/// — note the leading dot, which is ffmpeg's "any platform, this device index"
/// syntax.
#[must_use]
pub fn opencl_device_args(
    device_index: i32,
    device_vendor_name: Option<&str>,
    src_device_alias: Option<&str>,
    alias: &str,
) -> String {
    let device_index = device_index.max(0);
    let options = match src_device_alias.filter(|s| !s.is_empty()) {
        Some(src) => format!("@{src}"),
        None => match device_vendor_name.filter(|s| !s.is_empty()) {
            Some(vendor) => format!(":.{device_index},device_vendor=\"{vendor}\""),
            None => ":0.0".to_owned(),
        },
    };
    format!(" -init_hw_device opencl={alias}{options}")
}

/// ` -init_hw_device d3d11va=<alias>:<index|,vendor=id>`. Port of
/// `GetD3d11vaDeviceArgs`.
///
/// With a vendor id the argument reads `d3d11va=dx11:,vendor=0x8086` — a colon
/// immediately followed by a comma. That is upstream's exact output, not a
/// typo: the colon introduces the device spec and the empty field before the
/// comma means "any adapter matching the following options".
#[must_use]
pub fn d3d11va_device_args(
    device_index: i32,
    device_vendor_id: Option<&str>,
    alias: &str,
) -> String {
    let device_index = device_index.max(0);
    let options = match device_vendor_id.filter(|s| !s.is_empty()) {
        Some(vendor) => format!(",vendor={vendor}"),
        None => device_index.to_string(),
    };
    format!(" -init_hw_device d3d11va={alias}:{options}")
}

/// A configured render node together with whether it is usable.
///
/// The two travel as one value because they must agree: a path whose
/// `usable` flag is stale silently drops VAAPI selection to the
/// `vendor_id`/`kernel_driver` branch, which is a different device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderNode<'a> {
    path: Option<&'a str>,
    usable: bool,
}

impl<'a> RenderNode<'a> {
    /// Resolves a configured path against the filesystem.
    ///
    /// Port of C# `File.Exists(renderNodePath)`, which is **not** the same as
    /// "the path exists": `File.Exists` returns false for a **directory**,
    /// while returning true for the character devices render nodes actually
    /// are. (It also returns false when the path cannot be stat'ed at all, e.g.
    /// through an unsearchable parent directory — but a mode-000 *file* stats
    /// fine and counts as existing, in both implementations.) Reproducing that matters — an operator who
    /// pastes `/dev/dri` or a trailing slash must fall through to vendor/kernel
    /// pinning, not produce ` -init_hw_device vaapi=va:/dev/dri` that ffmpeg
    /// cannot open.
    ///
    /// This is the one function in the module that touches the filesystem; the
    /// argument builders stay pure and take the resolved value.
    #[must_use]
    pub fn resolve(path: Option<&'a str>) -> Self {
        let usable = path
            .filter(|p| !p.is_empty())
            .is_some_and(|p| std::fs::metadata(p).is_ok_and(|meta| !meta.is_dir()));
        Self { path, usable }
    }

    /// A render node with an explicitly stated usability, for tests and for
    /// callers that have already resolved it.
    ///
    /// "Usable" implies "has a non-empty path", so an absent or empty path
    /// forces `usable` to false rather than leaving a state
    /// [`resolve`](Self::resolve) can never produce — the VAAPI builder would
    /// otherwise take its "select this node" branch with nothing to select and
    /// emit a bare `vaapi=va:`.
    #[must_use]
    pub fn new(path: Option<&'a str>, usable: bool) -> Self {
        let usable = usable && path.is_some_and(|p| !p.is_empty());
        Self { path, usable }
    }

    /// The configured path, if any.
    #[must_use]
    pub fn path(self) -> Option<&'a str> {
        self.path
    }

    /// Whether the path may be used to select the device directly.
    #[must_use]
    pub fn usable(self) -> bool {
        self.usable
    }
}

/// How a VAAPI device should be selected.
///
/// The six selection parameters C# passes to `GetVaapiDeviceArgs`, gathered so
/// the builder reads as one decision rather than a wall of positional
/// arguments. Everything defaults to "unset", which is the common case: most
/// call sites set one or two fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VaapiDeviceSpec<'a> {
    /// The configured render node and whether it is usable.
    pub render_node: RenderNode<'a>,
    /// libva driver override, e.g. `iHD` or `i965`; behaves like the
    /// `LIBVA_DRIVER_NAME` environment variable.
    pub driver: Option<&'a str>,
    /// Kernel driver to match, e.g. `i915`. Lowest selection priority.
    pub kernel_driver: Option<&'a str>,
    /// PCI vendor id to match, e.g. `0x8086`. Needs ffmpeg 7.0.1 or newer.
    pub vendor_id: Option<&'a str>,
    /// Derive from an already-initialised device with this alias instead of
    /// selecting one; when set, every field above is ignored.
    pub src_device_alias: Option<&'a str>,
}

impl<'a> VaapiDeviceSpec<'a> {
    /// A spec selecting a configured render node.
    #[must_use]
    pub fn for_render_node(render_node: RenderNode<'a>) -> Self {
        Self {
            render_node,
            ..Self::default()
        }
    }

    /// Sets the libva driver override (`iHD` / `i965`).
    #[must_use]
    pub fn with_driver(mut self, driver: Option<&'a str>) -> Self {
        self.driver = driver;
        self
    }
}

/// ` -init_hw_device vaapi=…`. Port of `GetVaapiDeviceArgs`.
///
/// The device is chosen by strict priority — **render node path, then vendor
/// id, then kernel driver** — and `driver=` is appended on top of whichever
/// won.
///
/// The vendor-id form only appears on ffmpeg 7.0.1 and newer, which is where
/// `,vendor_id=` was added; older builds fall through to the kernel driver.
#[must_use]
pub fn vaapi_device_args(
    spec: &VaapiDeviceSpec<'_>,
    alias: &str,
    ffmpeg_version: Option<FfmpegVersion>,
) -> String {
    let have_vendor_id = spec.vendor_id.is_some_and(|v| !v.is_empty())
        && ffmpeg_version.is_some_and(|v| v >= MIN_FFMPEG_VAAPI_DEVICE_VENDOR_ID);

    let selector = if spec.render_node.usable() {
        spec.render_node.path().unwrap_or_default().to_owned()
    } else if have_vendor_id {
        format!(",vendor_id={}", spec.vendor_id.unwrap_or_default())
    } else {
        match spec.kernel_driver.filter(|s| !s.is_empty()) {
            Some(kd) => format!(",kernel_driver={kd}"),
            None => String::new(),
        }
    };

    // `driver` behaves like the LIBVA_DRIVER_NAME environment variable, and is
    // appended on top of whichever selector won.
    let driver_opts = match spec.driver.filter(|s| !s.is_empty()) {
        Some(driver) => format!("{selector},driver={driver}"),
        None => selector,
    };

    let options = match spec.src_device_alias.filter(|s| !s.is_empty()) {
        Some(src) => format!("@{src}"),
        None => {
            if driver_opts.is_empty() {
                String::new()
            } else {
                format!(":{driver_opts}")
            }
        }
    };
    format!(" -init_hw_device vaapi={alias}{options}")
}

/// ` -init_hw_device drm=<alias>:<render node>`. Port of `GetDrmDeviceArgs`.
///
/// **Accepted divergence.** Upstream falls back to [`DEFAULT_RENDER_NODE`] with
/// a null-coalesce (`renderNodePath ?? "/dev/dri/renderD128"`), so a *cleared*
/// device setting — an empty string, which the dashboard field allows — reaches
/// ffmpeg as the unusable ` -init_hw_device drm=dr:` and the transcode fails to
/// start. Every other device builder in this file uses `IsNullOrEmpty`; this one
/// is upstream's odd one out. Ferrofin treats empty as unset here too, so a
/// cleared field degrades to the default node instead of to a broken command
/// line. Recorded per the project rule on not porting Jellyfin's bugs.
#[must_use]
pub fn drm_device_args(render_node_path: Option<&str>, alias: &str) -> String {
    let node = render_node_path
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_RENDER_NODE);
    format!(" -init_hw_device drm={alias}:{node}")
}

/// The QSV device graph for this platform. Port of `GetQsvDeviceArgs`.
///
/// QSV is never initialised directly — it is always *derived* from a lower
/// level device, which is why this returns two `-init_hw_device` arguments:
///
/// - **Linux**: a VAAPI device pinned to the Intel iHD driver, then `qsv=qs@va`.
/// - **Windows**: a D3D11 device — by adapter index if `render_node_path` parses
///   as an integer, otherwise pinned to the Intel vendor id — then `qsv=qs@dx11`.
///
/// `None` on any other platform, matching the C# `return null`.
#[must_use]
pub fn qsv_device_args(
    render_node: RenderNode<'_>,
    alias: &str,
    platform: Platform,
    ffmpeg_version: Option<FfmpegVersion>,
) -> Option<String> {
    let arg = format!(" -init_hw_device qsv={alias}");
    match platform {
        Platform::Linux => Some(format!(
            "{}{arg}@{VAAPI_ALIAS}",
            vaapi_device_args(
                &VaapiDeviceSpec {
                    render_node,
                    driver: Some("iHD"),
                    kernel_driver: Some("i915"),
                    vendor_id: Some(VENDOR_ID_INTEL),
                    src_device_alias: None,
                },
                VAAPI_ALIAS,
                ffmpeg_version,
            )
        )),
        Platform::Windows => {
            // On Windows the configured "device" is an adapter index, not a path.
            // `int.TryParse(.., NumberStyles.Integer, ..)` tolerates
            // surrounding whitespace, so " 1 " selects adapter 1 upstream.
            let d3d11 = match render_node
                .path()
                .and_then(|p| p.trim().parse::<i32>().ok())
            {
                Some(index) => d3d11va_device_args(index, None, D3D11VA_ALIAS),
                None => d3d11va_device_args(0, Some(VENDOR_ID_INTEL), D3D11VA_ALIAS),
            };
            Some(format!("{d3d11}{arg}@{D3D11VA_ALIAS}"))
        }
        Platform::MacOs | Platform::Other => None,
    }
}

/// ` -filter_hw_device <alias>`, or nothing for no alias. Port of
/// `GetFilterHwDeviceArgs`.
///
/// This is what tells ffmpeg which of the initialised devices the *filter
/// graph* should run on, which is not always the one the decoder uses.
#[must_use]
pub fn filter_hw_device_args(alias: Option<&str>) -> String {
    match alias.filter(|s| !s.is_empty()) {
        Some(alias) => format!(" -filter_hw_device {alias}"),
        None => String::new(),
    }
}

/// Whether the configured QSV device string selects the D3D11 adapter by index.
///
/// Exposed because the Windows QSV filter chain has to know which of the two
/// shapes [`qsv_device_args`] produced.
#[must_use]
pub fn qsv_device_is_adapter_index(render_node_path: Option<&str>) -> bool {
    render_node_path.is_some_and(|p| p.trim().parse::<i32>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg 7.0.1 — new enough for the `,vendor_id=` VAAPI option.
    const V701: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

    /// ffmpeg 6.0 — too old for `,vendor_id=`.
    const V60: FfmpegVersion = FfmpegVersion::new(6, 0);

    // Every expectation below is hand-derived from the C# at
    // MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs (10.11.z lines
    // 809-961). Upstream ships no tests for these builders.

    #[test]
    fn the_selection_free_devices_are_just_an_alias() {
        assert_eq!(rkmpp_device_args(RKMPP_ALIAS), " -init_hw_device rkmpp=rk");
        assert_eq!(
            videotoolbox_device_args(VIDEOTOOLBOX_ALIAS),
            " -init_hw_device videotoolbox=vt"
        );
    }

    #[test]
    fn cuda_takes_an_index_and_clamps_a_negative_one() {
        assert_eq!(
            cuda_device_args(0, CUDA_ALIAS),
            " -init_hw_device cuda=cu:0"
        );
        assert_eq!(
            cuda_device_args(2, CUDA_ALIAS),
            " -init_hw_device cuda=cu:2"
        );
        assert_eq!(
            cuda_device_args(-1, CUDA_ALIAS),
            " -init_hw_device cuda=cu:0",
            "a negative index means unset and becomes 0"
        );
    }

    #[test]
    fn vulkan_prefers_a_source_device_then_a_name_then_an_index() {
        assert_eq!(
            vulkan_device_args(0, None, Some(DRM_ALIAS), VULKAN_ALIAS),
            " -init_hw_device vulkan=vk@dr"
        );
        assert_eq!(
            vulkan_device_args(1, Some("Radeon"), None, VULKAN_ALIAS),
            " -init_hw_device vulkan=vk:\"Radeon\""
        );
        assert_eq!(
            vulkan_device_args(1, None, None, VULKAN_ALIAS),
            " -init_hw_device vulkan=vk:1"
        );
        // A source alias wins even when a name is also given.
        assert_eq!(
            vulkan_device_args(1, Some("Radeon"), Some(DRM_ALIAS), VULKAN_ALIAS),
            " -init_hw_device vulkan=vk@dr"
        );
    }

    #[test]
    fn opencl_defaults_to_platform_zero_device_zero() {
        assert_eq!(
            opencl_device_args(0, None, None, OPENCL_ALIAS),
            " -init_hw_device opencl=ocl:0.0"
        );
        assert_eq!(
            opencl_device_args(0, None, Some(VAAPI_ALIAS), OPENCL_ALIAS),
            " -init_hw_device opencl=ocl@va"
        );
        // The vendor form keeps the leading dot: "any platform, this device".
        assert_eq!(
            opencl_device_args(0, Some(OPENCL_VENDOR_AMD), None, OPENCL_ALIAS),
            " -init_hw_device opencl=ocl:.0,device_vendor=\"Advanced Micro Devices\""
        );
    }

    #[test]
    fn d3d11va_takes_an_index_or_pins_a_vendor() {
        assert_eq!(
            d3d11va_device_args(0, None, D3D11VA_ALIAS),
            " -init_hw_device d3d11va=dx11:0"
        );
        assert_eq!(
            d3d11va_device_args(3, None, D3D11VA_ALIAS),
            " -init_hw_device d3d11va=dx11:3"
        );
        // Colon-then-comma is upstream's exact output for the vendor form.
        assert_eq!(
            d3d11va_device_args(0, Some(VENDOR_ID_INTEL), D3D11VA_ALIAS),
            " -init_hw_device d3d11va=dx11:,vendor=0x8086"
        );
        assert_eq!(
            d3d11va_device_args(0, Some(VENDOR_ID_AMD), D3D11VA_ALIAS),
            " -init_hw_device d3d11va=dx11:,vendor=0x1002"
        );
    }

    #[test]
    fn vaapi_prefers_an_existing_render_node_over_everything() {
        assert_eq!(
            vaapi_device_args(
                &VaapiDeviceSpec {
                    render_node: RenderNode::new(Some("/dev/dri/renderD128"), true),
                    driver: Some("iHD"),
                    kernel_driver: Some("i915"),
                    vendor_id: Some(VENDOR_ID_INTEL),
                    src_device_alias: None,
                },
                VAAPI_ALIAS,
                Some(V701),
            ),
            " -init_hw_device vaapi=va:/dev/dri/renderD128,driver=iHD"
        );
    }

    #[test]
    fn vaapi_falls_back_to_vendor_id_then_kernel_driver() {
        // No node on disk, ffmpeg new enough: pin by vendor id.
        assert_eq!(
            vaapi_device_args(
                &VaapiDeviceSpec {
                    render_node: RenderNode::new(Some("/dev/dri/renderD128"), false),
                    driver: Some("iHD"),
                    kernel_driver: Some("i915"),
                    vendor_id: Some(VENDOR_ID_INTEL),
                    src_device_alias: None,
                },
                VAAPI_ALIAS,
                Some(V701),
            ),
            " -init_hw_device vaapi=va:,vendor_id=0x8086,driver=iHD"
        );
        // Same inputs on ffmpeg 6.0, which has no `,vendor_id=`: kernel driver.
        assert_eq!(
            vaapi_device_args(
                &VaapiDeviceSpec {
                    render_node: RenderNode::new(Some("/dev/dri/renderD128"), false),
                    driver: Some("iHD"),
                    kernel_driver: Some("i915"),
                    vendor_id: Some(VENDOR_ID_INTEL),
                    src_device_alias: None,
                },
                VAAPI_ALIAS,
                Some(V60),
            ),
            " -init_hw_device vaapi=va:,kernel_driver=i915,driver=iHD"
        );
        // An unknown ffmpeg version is treated as too old, like every gate.
        assert_eq!(
            vaapi_device_args(
                &VaapiDeviceSpec {
                    kernel_driver: Some("i915"),
                    vendor_id: Some(VENDOR_ID_INTEL),
                    ..VaapiDeviceSpec::default()
                },
                VAAPI_ALIAS,
                None,
            ),
            " -init_hw_device vaapi=va:,kernel_driver=i915"
        );
    }

    #[test]
    fn vaapi_with_only_a_driver_still_emits_the_leading_comma() {
        // This is the shape the iHD branch produces when the configured render
        // node does not exist: no device selector, just the driver override.
        assert_eq!(
            vaapi_device_args(
                &VaapiDeviceSpec {
                    render_node: RenderNode::new(Some("/dev/dri/renderD128"), false),
                    driver: Some("iHD"),
                    ..VaapiDeviceSpec::default()
                },
                VAAPI_ALIAS,
                Some(V701),
            ),
            " -init_hw_device vaapi=va:,driver=iHD"
        );
    }

    #[test]
    fn vaapi_derived_from_another_device_ignores_the_driver_options() {
        // The AMD Vulkan path derives VAAPI from the DRM device.
        assert_eq!(
            vaapi_device_args(
                &VaapiDeviceSpec {
                    src_device_alias: Some(DRM_ALIAS),
                    ..VaapiDeviceSpec::default()
                },
                VAAPI_ALIAS,
                Some(V701),
            ),
            " -init_hw_device vaapi=va@dr"
        );
    }

    #[test]
    fn vaapi_with_nothing_to_say_emits_a_bare_device() {
        // The unknown-vendor AMD branch: no driver override, no selector.
        assert_eq!(
            vaapi_device_args(&VaapiDeviceSpec::default(), VAAPI_ALIAS, Some(V701)),
            " -init_hw_device vaapi=va"
        );
    }

    #[test]
    fn drm_defaults_to_the_first_render_node() {
        assert_eq!(
            drm_device_args(Some("/dev/dri/renderD129"), DRM_ALIAS),
            " -init_hw_device drm=dr:/dev/dri/renderD129"
        );
        assert_eq!(
            drm_device_args(None, DRM_ALIAS),
            " -init_hw_device drm=dr:/dev/dri/renderD128"
        );
        // The accepted divergence: upstream's null-coalesce would emit the
        // unusable ` -init_hw_device drm=dr:` for a cleared device setting.
        assert_eq!(
            drm_device_args(Some(""), DRM_ALIAS),
            " -init_hw_device drm=dr:/dev/dri/renderD128"
        );
    }

    #[test]
    fn qsv_on_linux_derives_from_a_vaapi_device() {
        assert_eq!(
            qsv_device_args(
                RenderNode::new(Some("/dev/dri/renderD128"), true),
                QSV_ALIAS,
                Platform::Linux,
                Some(V701)
            )
            .as_deref(),
            Some(
                " -init_hw_device vaapi=va:/dev/dri/renderD128,driver=iHD \
                 -init_hw_device qsv=qs@va"
            )
        );
    }

    #[test]
    fn qsv_on_linux_pins_intel_when_the_render_node_is_absent() {
        // The case the pinning constants exist for: the configured node is not
        // on disk, so selection falls to vendor id (ffmpeg >= 7.0.1)...
        assert_eq!(
            qsv_device_args(
                RenderNode::new(Some("/dev/dri/renderD128"), false),
                QSV_ALIAS,
                Platform::Linux,
                Some(V701)
            )
            .as_deref(),
            Some(
                " -init_hw_device vaapi=va:,vendor_id=0x8086,driver=iHD \
                 -init_hw_device qsv=qs@va"
            )
        );
        // ...or to the kernel driver on a build without `,vendor_id=`.
        assert_eq!(
            qsv_device_args(
                RenderNode::new(Some("/dev/dri/renderD128"), false),
                QSV_ALIAS,
                Platform::Linux,
                Some(V60)
            )
            .as_deref(),
            Some(
                " -init_hw_device vaapi=va:,kernel_driver=i915,driver=iHD \
                 -init_hw_device qsv=qs@va"
            )
        );
    }

    #[test]
    fn qsv_on_windows_derives_from_a_d3d11_device() {
        // A numeric device string is an adapter index.
        assert_eq!(
            qsv_device_args(
                RenderNode::new(Some("1"), false),
                QSV_ALIAS,
                Platform::Windows,
                Some(V701)
            )
            .as_deref(),
            Some(" -init_hw_device d3d11va=dx11:1 -init_hw_device qsv=qs@dx11")
        );
        // Anything else pins the Intel adapter by vendor id.
        assert_eq!(
            qsv_device_args(
                RenderNode::new(None, false),
                QSV_ALIAS,
                Platform::Windows,
                Some(V701)
            )
            .as_deref(),
            Some(" -init_hw_device d3d11va=dx11:,vendor=0x8086 -init_hw_device qsv=qs@dx11")
        );
        assert_eq!(
            qsv_device_args(
                RenderNode::new(Some(""), false),
                QSV_ALIAS,
                Platform::Windows,
                Some(V701)
            )
            .as_deref(),
            Some(" -init_hw_device d3d11va=dx11:,vendor=0x8086 -init_hw_device qsv=qs@dx11")
        );
    }

    #[test]
    fn qsv_has_no_device_graph_off_linux_and_windows() {
        assert_eq!(
            qsv_device_args(
                RenderNode::new(None, false),
                QSV_ALIAS,
                Platform::MacOs,
                Some(V701)
            ),
            None
        );
        assert_eq!(
            qsv_device_args(
                RenderNode::new(None, false),
                QSV_ALIAS,
                Platform::Other,
                Some(V701)
            ),
            None
        );
    }

    #[test]
    fn qsv_adapter_index_detection_matches_the_windows_branch() {
        assert!(qsv_device_is_adapter_index(Some("0")));
        assert!(qsv_device_is_adapter_index(Some("12")));
        assert!(
            qsv_device_is_adapter_index(Some(" 1 ")),
            "whitespace tolerated"
        );
        assert!(!qsv_device_is_adapter_index(Some("/dev/dri/renderD128")));
        assert!(!qsv_device_is_adapter_index(Some("")));
        assert!(!qsv_device_is_adapter_index(None));
    }

    // The paths below (`/`, `/dev`, `/dev/null`) are unix facts; the predicate
    // itself is platform-independent and its pure half is covered above.
    #[cfg(unix)]
    #[test]
    fn resolving_a_render_node_reproduces_file_exists_not_path_exists() {
        // A path that is not there must not claim to be.
        let missing = RenderNode::resolve(Some("/dev/dri/renderD-absent"));
        assert!(!missing.usable());
        assert_eq!(missing.path(), Some("/dev/dri/renderD-absent"));

        // An unset or cleared device has nothing to stat.
        assert!(!RenderNode::resolve(None).usable());
        assert!(!RenderNode::resolve(Some("")).usable());

        // THE CASE THAT SEPARATES `File.Exists` FROM `Path::exists`: a
        // directory is NOT a usable device. `/` and `/dev` are always
        // directories, so an operator pasting `/dev/dri` must fall through to
        // vendor/kernel pinning rather than produce a device string ffmpeg
        // cannot open.
        assert!(!RenderNode::resolve(Some("/")).usable());
        assert!(!RenderNode::resolve(Some("/dev")).usable());

        // A real non-directory is usable. `/dev/null` is a character device,
        // the same class of node as `/dev/dri/renderD128`.
        assert!(RenderNode::resolve(Some("/dev/null")).usable());

        // The spec carries the pair, and the driver rides along beside it.
        let spec = VaapiDeviceSpec::for_render_node(RenderNode::resolve(Some("/dev/null")))
            .with_driver(Some("iHD"));
        assert_eq!(spec.driver, Some("iHD"));
        assert!(spec.render_node.usable());
        assert_eq!(spec.render_node.path(), Some("/dev/null"));
    }

    #[test]
    fn a_render_node_cannot_be_usable_without_a_path() {
        // The invalid state the VAAPI builder would mis-handle: "use this
        // node" with no node. `new` normalises it away.
        assert!(!RenderNode::new(None, true).usable());
        assert!(!RenderNode::new(Some(""), true).usable());
        assert!(RenderNode::new(Some("/dev/dri/renderD128"), true).usable());
        assert!(!RenderNode::new(Some("/dev/dri/renderD128"), false).usable());
        assert_eq!(RenderNode::default(), RenderNode::new(None, false));
    }

    #[test]
    fn the_filter_device_argument_is_optional() {
        assert_eq!(
            filter_hw_device_args(Some(CUDA_ALIAS)),
            " -filter_hw_device cu"
        );
        assert_eq!(filter_hw_device_args(None), "");
        assert_eq!(filter_hw_device_args(Some("")), "");
    }
}
