//! The transcode argument builder and direct-play decision.
//!
//! Ports `MediaBrowser.Controller.MediaEncoding.EncodingHelper` plus the
//! minimal job-state structs it reads.
//!
//! - [`transcode_state`] holds the prerequisite [`EncodingJobInfo`] /
//!   [`BaseEncodingJobOptions`] structs (a ported subset) and the
//!   [`EncoderCapabilities`] seam.
//! - [`helper`] holds [`EncodingHelper`] — the software path: encoder
//!   selection, stream mapping, bitrate/quality/thread params, and
//!   `can_stream_copy_{video,audio}`.
//! - [`hw`] holds the hardware-acceleration half. Landed so far: the probed
//!   environment and the version gates. The device-init graphs, hardware
//!   decoder selection, and the per-vendor filter chains are the named work
//!   items in `brain/plans/PLAN_HWACCEL.md`; see that module's own docs.
//!
//! There is **no upstream parity oracle** for this unit — the `EncodingHelper`
//! tests live in the out-of-scope `Jellyfin.Controller` test project, and
//! upstream ships none at all for the hardware builders — so the tests here
//! transliterate hand-derived expectations from the C# logic.

pub mod helper;
pub mod hw;
pub mod transcode_state;

pub use helper::EncodingHelper;
pub use transcode_state::{
    BaseEncodingJobOptions, EncoderCapabilities, EncodingJobInfo, NoOptionalEncoders,
    TranscodeDisplayNames,
};
