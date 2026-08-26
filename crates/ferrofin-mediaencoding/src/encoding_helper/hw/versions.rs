//! The ffmpeg and Linux-kernel version gates the hardware-acceleration matrix
//! consults.
//!
//! Port of the `_minFFmpeg*` / `_min*Kernel*i915*` / `_minKernelVersionAmd*`
//! readonly fields of C# `EncodingHelper` (10.11.z lines 64–87), plus
//! `EncoderValidator._minFFmpegMultiThreadedCli`. Every value is upstream-fixed
//! — an ffmpeg release either grew the option or it did not — so these are
//! constants, never configuration.
//!
//! Ferrofin's container image ships jellyfin-ffmpeg 7, which clears every gate
//! here; the gates still matter because an operator may point
//! `FERROFIN_FFMPEG_PATH` at any build from 4.4 up.

use crate::encoder::FfmpegVersion;

/// `-hwaccel` alone implies the decoder; below this the decoder must be named
/// explicitly (`-c:v av1`). Port of `_minFFmpegImplicitHwaccel` (6.0).
pub const MIN_FFMPEG_IMPLICIT_HWACCEL: FfmpegVersion = FfmpegVersion::new(6, 0);

/// `-vsync` is deprecated from here on and `-fps_mode` replaces it. Port of the
/// inline `new Version(5, 1)` in `EncodingHelper.GetVideoSyncOption` — upstream
/// writes the literal rather than naming a field, but it is the same kind of
/// gate as its neighbours here.
pub const MIN_FFMPEG_FPS_MODE_OPTION: FfmpegVersion = FfmpegVersion::new(5, 1);

/// nvdec can skip its internal frame copy via `-hwaccel_flags +unsafe_output`.
/// Port of `_minFFmpegHwaUnsafeOutput` (6.0).
pub const MIN_FFMPEG_HWA_UNSAFE_OUTPUT: FfmpegVersion = FfmpegVersion::new(6, 0);

/// `tonemap_opencl`/`tonemap_cuda` accept `tonemap_mode=max|rgb`. Port of
/// `_minFFmpegOclCuTonemapMode` (5.1.3).
pub const MIN_FFMPEG_OCL_CU_TONEMAP_MODE: FfmpegVersion = FfmpegVersion::with_build(5, 1, 3);

/// `libsvtav1` accepts `-svtav1-params`. Port of `_minFFmpegSvtAv1Params` (5.1).
pub const MIN_FFMPEG_SVT_AV1_PARAMS: FfmpegVersion = FfmpegVersion::new(5, 1);

/// The VAAPI H.26x encoders emit A/53 closed-caption SEI. Port of
/// `_minFFmpegVaapiH26xEncA53CcSei` (6.0).
pub const MIN_FFMPEG_VAAPI_H26X_ENC_A53_CC_SEI: FfmpegVersion = FfmpegVersion::new(6, 0);

/// The `-readrate` input option exists. Port of `_minFFmpegReadrateOption` (5.0).
pub const MIN_FFMPEG_READRATE_OPTION: FfmpegVersion = FfmpegVersion::new(5, 0);

/// VideoToolbox hardware surfaces (`-hwaccel_output_format videotoolbox_vld`)
/// work. Port of `_minFFmpegWorkingVtHwSurface` (7.0.1).
pub const MIN_FFMPEG_WORKING_VT_HW_SURFACE: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

/// The `-display_rotation` input option exists. Port of
/// `_minFFmpegDisplayRotationOption` (6.0).
pub const MIN_FFMPEG_DISPLAY_ROTATION_OPTION: FfmpegVersion = FfmpegVersion::new(6, 0);

/// `tonemap_*` accept the advanced `tonemap_mode=lum|itp`. Port of
/// `_minFFmpegAdvancedTonemapMode` (7.0.1).
pub const MIN_FFMPEG_ADVANCED_TONEMAP_MODE: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

/// The VAAPI↔Vulkan interop semantics changed. Port of
/// `_minFFmpegAlteredVaVkInterop` (7.0.1).
pub const MIN_FFMPEG_ALTERED_VA_VK_INTEROP: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

/// `vpp_qsv` accepts `tonemap=1`. Port of `_minFFmpegQsvVppTonemapOption` (7.0.1).
pub const MIN_FFMPEG_QSV_VPP_TONEMAP_OPTION: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

/// `vpp_qsv` accepts an output colour range. Port of
/// `_minFFmpegQsvVppOutRangeOption` (7.0.1).
pub const MIN_FFMPEG_QSV_VPP_OUT_RANGE_OPTION: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

/// `-init_hw_device vaapi=…` accepts `,vendor_id=`. Port of
/// `_minFFmpegVaapiDeviceVendorId` (7.0.1).
pub const MIN_FFMPEG_VAAPI_DEVICE_VENDOR_ID: FfmpegVersion = FfmpegVersion::with_build(7, 0, 1);

/// `vpp_qsv` accepts `scale_mode=hq`. Port of
/// `_minFFmpegQsvVppScaleModeOption` (6.0).
pub const MIN_FFMPEG_QSV_VPP_SCALE_MODE_OPTION: FfmpegVersion = FfmpegVersion::new(6, 0);

/// `hevc_rkmpp` can parse Dolby Vision RPUs. Port of
/// `_minFFmpegRkmppHevcDecDoviRpu` (7.1.1).
pub const MIN_FFMPEG_RKMPP_HEVC_DEC_DOVI_RPU: FfmpegVersion = FfmpegVersion::with_build(7, 1, 1);

/// The `-readrate_catchup` option exists. Port of
/// `_minFFmpegReadrateCatchupOption` (8.0).
pub const MIN_FFMPEG_READRATE_CATCHUP_OPTION: FfmpegVersion = FfmpegVersion::new(8, 0);

/// The ffmpeg CLI became multi-threaded and so less sensitive to stdin timing.
/// Port of `EncoderValidator._minFFmpegMultiThreadedCli` (7.0).
pub const MIN_FFMPEG_MULTI_THREADED_CLI: FfmpegVersion = FfmpegVersion::new(7, 0);

/// First kernel exhibiting the i915 hang. Port of `_minKerneli915Hang` (5.18).
///
/// The hang was fixed by Linux 6.2 (commit `3f882f2`); the workaround applies to
/// the closed range \[[`MIN_KERNEL_I915_HANG`], [`MAX_KERNEL_I915_HANG`]\]
/// except for 6.0.x at or above [`MIN_FIXED_KERNEL_60_I915_HANG`].
pub const MIN_KERNEL_I915_HANG: FfmpegVersion = FfmpegVersion::new(5, 18);

/// Last kernel exhibiting the i915 hang. Port of `_maxKerneli915Hang` (6.1.3).
pub const MAX_KERNEL_I915_HANG: FfmpegVersion = FfmpegVersion::with_build(6, 1, 3);

/// The 6.0.x point release that backported the i915 hang fix. Port of
/// `_minFixedKernel60i915Hang` (6.0.18).
pub const MIN_FIXED_KERNEL_60_I915_HANG: FfmpegVersion = FfmpegVersion::with_build(6, 0, 18);

/// Kernel needed for the AMD VAAPI↔Vulkan DRM-format-modifier interop path.
/// Port of `_minKernelVersionAmdVkFmtModifier` (5.15).
pub const MIN_KERNEL_VERSION_AMD_VK_FMT_MODIFIER: FfmpegVersion = FfmpegVersion::new(5, 15);
