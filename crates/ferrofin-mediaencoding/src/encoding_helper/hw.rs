//! The hardware-acceleration half of `EncodingHelper`.
//!
//! Jellyfin builds one ffmpeg command line per playback decision, and on a
//! machine with a GPU most of that command line is hardware plumbing:
//! `-init_hw_device` graphs, `-hwaccel` selection, and per-vendor filter chains
//! that upload, scale, deinterlace, tonemap, and overlay subtitles on the GPU.
//! That logic is ~5,900 lines of C# spread across `EncodingHelper`,
//! `EncoderValidator`, and `MediaEncoder`.
//!
//! **All of it is ported and wired**, for the three accelerators Ferrofin
//! supports:
//!
//! - [`capabilities`] — the resolved environment: what the running ffmpeg can
//!   do, which OS it is on, and which kernel is underneath.
//! - [`versions`] — the ffmpeg and kernel version gates the builders consult.
//! - [`support`] — "is this backend usable?", the predicates every branch asks
//!   before emitting an argument.
//! - [`device_init`] — the `-init_hw_device` graphs.
//! - [`encoder`] — hardware encoder selection.
//! - [`decoder`] — hardware decoder selection: `-hwaccel` and `-c:v`.
//! - [`input_args`] — the dispatcher that assembles everything before `-i`.
//! - [`tonemap`] — the five tonemapping paths and the colour-property params.
//! - [`filters`] — the shared filter fragments the chains are built from.
//! - [`sw_chain`] — the software filter chain, the shared chain input, and the
//!   assembly that turns any chain into ffmpeg arguments.
//! - [`nvidia`] — the NVENC / CUDA filter chain.
//! - [`vaapi`] — the three VAAPI chains (Intel iHD, limited/AMD, and the AMD
//!   Vulkan one).
//! - [`qsv`] — the QSV chains, Linux (VAAPI-derived) and Windows (D3D11).
//! - [`quality`] — the encoder quality preamble: low-power encoding, the i915
//!   hang workaround, and the per-encoder quality/bitrate arms.
//!
//! `apps/ferrofin-server`'s planner dispatches to these per accelerator, and
//! [`super::bitstream`] handles the Dolby Vision / HDR10+ metadata the copy
//! path has to strip. **AMF, VideoToolbox, RKMPP and V4L2M2M have no chain
//! here** — see CLAUDE.md's "Current scope": there is no hardware to verify
//! them on, so selecting one falls back to a full software transcode and logs
//! a warning rather than emitting a pipeline nobody has run.
//!
//! Two rules shape everything in this module:
//!
//! - **The OS is data.** Every `OperatingSystem.IsWindows()` in the C# becomes a
//!   [`Platform`] value carried in [`FfmpegCapabilities`], never a `cfg!`. The
//!   Windows and macOS branches must be exercisable from a Linux test runner,
//!   because that is the only runner this project has.
//! - **The arguments are the contract.** Each builder is a pure function
//!   producing the same argument strings C# produces. Upstream ships **no**
//!   tests for any of these builders, so the C# source itself is the oracle and
//!   every golden is hand-derived from it rather than transliterated from an
//!   upstream case.

pub mod capabilities;
pub mod decoder;
pub mod device_init;
pub mod encoder;
pub mod filters;
pub mod input_args;
pub mod nvidia;
pub mod qsv;
pub mod quality;
pub mod support;
pub mod sw_chain;
pub mod tonemap;
pub mod vaapi;
pub mod versions;

/// `string.Contains(x, StringComparison.OrdinalIgnoreCase)`.
///
/// Every chain identifies decoders and encoders by substring the way the C#
/// does, so this lives here rather than being copied into each vendor module.
pub(super) fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

pub use capabilities::{
    BsfOption, FfmpegCapabilities, FfmpegCapabilitiesBuilder, FilterOption, Platform,
    parse_os_release,
};
