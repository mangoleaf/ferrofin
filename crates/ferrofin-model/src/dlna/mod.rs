//! Port of `MediaBrowser.Model.Dlna` — the DLNA/device-profile model.
//!
//! The standalone enums, the device-profile model structs
//! ([`DeviceProfile`] and its constituent profiles), and the `StreamBuilder`
//! transcode/direct-play decision engine live here.
//!
//! Every public item is re-exported at the `crate::dlna` root so consumers can
//! use the flat namespace mirroring the C# `MediaBrowser.Model.Dlna` namespace.

pub mod codec_profile;
pub mod condition_processor;
pub mod container_profile;
pub mod device_profile;
pub mod direct_play_profile;
pub mod enums;
pub mod media_options;
pub mod profile_condition;
pub mod resolution;
pub mod stream_builder;
pub mod stream_info;
pub mod subtitle_profile;
pub mod subtitle_stream_info;
pub mod transcoder_support;
pub mod transcoding_profile;

pub use codec_profile::CodecProfile;
pub use condition_processor::ConditionProcessor;
pub use container_profile::ContainerProfile;
pub use device_profile::DeviceProfile;
pub use direct_play_profile::DirectPlayProfile;
pub use enums::{
    CodecType, DlnaProfileType, EncodingContext, PlaybackErrorCode, ProfileConditionType,
    ProfileConditionValue, SubtitleDeliveryMethod, TranscodeSeekInfo,
};
pub use media_options::MediaOptions;
pub use profile_condition::ProfileCondition;
pub use resolution::{ResolutionConfiguration, ResolutionNormalizer, ResolutionOptions};
pub use stream_builder::StreamBuilder;
pub use stream_info::StreamInfo;
pub use subtitle_profile::SubtitleProfile;
pub use subtitle_stream_info::SubtitleStreamInfo;
pub use transcoder_support::TranscoderSupport;
pub use transcoding_profile::TranscodingProfile;
