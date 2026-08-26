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
//! - [`hw`] holds the hardware-acceleration half: the probed environment, the
//!   version gates, the device-init graphs, decoder/encoder selection, and the
//!   NVENC, VAAPI and QSV filter chains. See that module's own docs.
//! - [`bitstream`] holds the Dolby Vision / HDR10+ metadata removal the
//!   stream-copy decision and the copy path both consult.
//!
//! There is **no upstream parity oracle** for this unit — the `EncodingHelper`
//! tests live in the out-of-scope `Jellyfin.Controller` test project, and
//! upstream ships none at all for the hardware builders — so the tests here
//! transliterate hand-derived expectations from the C# logic.

pub mod bitstream;
pub mod helper;
pub mod hw;
pub mod transcode_state;

pub use helper::EncodingHelper;
pub use transcode_state::{
    BaseEncodingJobOptions, EncoderCapabilities, EncodingJobInfo, NoOptionalEncoders,
    TranscodeDisplayNames,
};
