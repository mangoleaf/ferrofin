//! ffprobe JSON -> [`ferrofin_model::media_info::MediaInfo`] normalization.
//!
//! Port of `MediaBrowser.MediaEncoding.Probing`: the ffprobe DTOs, the
//! [`ff_probe_helpers`] tag utilities, and the [`ProbeResultNormalizer`]. This
//! is a pure transformation — no ffmpeg/ffprobe process is spawned here.

pub mod dtos;
pub mod ff_probe_helpers;
pub mod localization;
pub mod probe_result_normalizer;

pub use dtos::{
    CodecType, InternalMediaInfoResult, MediaChapter, MediaFormatInfo, MediaFrameInfo,
    MediaFrameSideDataInfo, MediaStreamInfo, MediaStreamInfoSideData,
};
pub use localization::{LocalizationManager, PassthroughLocalization};
pub use probe_result_normalizer::{
    ProbeResultNormalizer, get_estimated_audio_bitrate, get_frame_rate, is_near_square_pixel_sar,
};
