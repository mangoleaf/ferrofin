//! The hardware-acceleration half of `EncodingHelper`.
//!
//! Jellyfin builds one ffmpeg command line per playback decision, and on a
//! machine with a GPU most of that command line is hardware plumbing:
//! `-init_hw_device` graphs, `-hwaccel` selection, and per-vendor filter chains
//! that upload, scale, deinterlace, tonemap, and overlay subtitles on the GPU.
//! That logic is ~5,900 lines of C# spread across `EncodingHelper`,
//! `EncoderValidator`, and `MediaEncoder`.
//!
//! **What lives here today** is the foundation the rest of that port stands on:
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
//!
//! That covers everything before ffmpeg's `-i`, the filter fragments the graphs
//! after it are built from, the software chain itself, and the assembly that
//! turns any chain into arguments. Still to come, as named work items in
//! `brain/plans/PLAN_HWACCEL.md`: five more per-vendor filter chains and the
//! switch that selects between them (phases 4–7), the Dolby Vision / HDR10+
//! bitstream handling (phase 8), and the accelerated trickplay path (phase 9).
//! Nothing here is wired into the planner yet — phase 3b does that, and until
//! then the software path in [`super::helper`] is what runs.
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
