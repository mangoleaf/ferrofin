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
//!
//! Still to come, as named work items in `brain/plans/PLAN_HWACCEL.md`:
//! hardware decoder selection and the input-argument dispatcher (rest of phase
//! 1), the shared filter helpers and tonemapping (phase 2), and the per-vendor
//! filter chains (phases 3–7). Until each lands, the software path in
//! [`super::helper`] is what runs.
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
pub mod device_init;
pub mod encoder;
pub mod support;
pub mod versions;

pub use capabilities::{
    BsfOption, FfmpegCapabilities, FfmpegCapabilitiesBuilder, FilterOption, Platform,
    parse_os_release,
};
