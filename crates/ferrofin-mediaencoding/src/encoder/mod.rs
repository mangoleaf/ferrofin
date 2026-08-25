//! ffmpeg/ffprobe encoder discovery, validation, and argument building.
//!
//! Port of `MediaBrowser.MediaEncoding.Encoder`. This unit lands the *pure*
//! half: the [`EncoderValidator`] version parsing/validation and capability
//! probe parsing (codecs, hwaccels, filters, filter/bitstream-filter options,
//! and the VAAPI/Vulkan device probes), the [`FfmpegVersion`]
//! `System.Version` model, and the [`encoding_utils`] input argument builders.
//! The un-mockable ffmpeg process spawn sits behind the [`Transcoder`] seam,
//! and the startup probe round is driven by the composition root.

pub mod encoder_validator;
pub mod encoding_utils;
pub mod media_encoder;
pub mod tokio_transcoder;
pub mod transcoder;
pub mod trickplay_extractor;
pub mod version;

#[cfg(test)]
mod test_data;

pub use encoder_validator::{
    EncoderValidator, MAX_VERSION, MIN_VERSION, REQUIRED_DECODERS, REQUIRED_ENCODERS,
    REQUIRED_FILTERS, VULKAN_EXTERNAL_MEMORY_DMA_BUF_EXTS, VULKAN_IMAGE_DRM_FMT_MODIFIER_EXTS,
    bsf_option_probe, filter_option_probe,
};
pub use encoding_utils::{get_input_argument, get_input_argument_multi, normalize_path};
pub use media_encoder::{MediaEncoderConfig, MediaEncoderImpl};
pub use tokio_transcoder::TokioTranscoder;
pub use transcoder::Transcoder;
pub use trickplay_extractor::TrickplayFrameExtractorImpl;
pub use version::FfmpegVersion;
