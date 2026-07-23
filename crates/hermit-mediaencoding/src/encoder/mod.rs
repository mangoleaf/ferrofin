//! ffmpeg/ffprobe encoder discovery, validation, and argument building.
//!
//! Port of `MediaBrowser.MediaEncoding.Encoder`. This unit lands the *pure*
//! half: the [`EncoderValidator`] version parsing/validation, the
//! [`FfmpegVersion`] `System.Version` model, and the [`encoding_utils`] input
//! argument builders. The un-mockable ffmpeg process spawn sits behind the
//! [`Transcoder`] seam. The hardware `Check*` device probes and the
//! capability-probe methods (codecs/hwaccels/filters) are deferred.

pub mod encoder_validator;
pub mod encoding_utils;
pub mod media_encoder;
pub mod tokio_transcoder;
pub mod transcoder;
pub mod version;

#[cfg(test)]
mod test_data;

pub use encoder_validator::{EncoderValidator, MAX_VERSION, MIN_VERSION};
pub use encoding_utils::{get_input_argument, get_input_argument_multi, normalize_path};
pub use media_encoder::{MediaEncoderConfig, MediaEncoderImpl};
pub use tokio_transcoder::TokioTranscoder;
pub use transcoder::Transcoder;
pub use version::FfmpegVersion;
