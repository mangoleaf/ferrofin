//! The core software-transcode argument builder and direct-play decision.
//!
//! Ports the *software slice* of `MediaBrowser.Controller.MediaEncoding.
//! EncodingHelper` plus the minimal job-state structs it reads. This is the last
//! unit of the `hermit-mediaencoding` port; nothing else depends on it.
//!
//! - [`transcode_state`] holds the prerequisite [`EncodingJobInfo`] /
//!   [`BaseEncodingJobOptions`] structs (a ported subset) and the
//!   [`EncoderCapabilities`] seam.
//! - [`helper`] holds [`EncodingHelper`] — encoder selection, stream mapping,
//!   bitrate/quality/thread params, and `can_stream_copy_{video,audio}`.
//!
//! The full hardware-acceleration matrix, tonemapping/HDR filters, and hardware
//! scale/filter chains are deferred (see `brain/DEFERRED.md`). There is **no
//! upstream parity oracle** for this unit — the `EncodingHelper` tests live in
//! the out-of-scope `Jellyfin.Controller` test project — so the tests here
//! transliterate hand-derived expectations from the C# logic.

pub mod helper;
pub mod transcode_state;

pub use helper::EncodingHelper;
pub use transcode_state::{
    BaseEncodingJobOptions, EncoderCapabilities, EncodingJobInfo, NoOptionalEncoders,
};
