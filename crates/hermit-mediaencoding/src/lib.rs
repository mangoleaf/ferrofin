//! ffmpeg/ffprobe media encoding for Hermit — port of `MediaBrowser.MediaEncoding`
//! (+ the arg-building core of `EncodingHelper`).
//!
//! Implements the `hermit-traits` `MediaEncoder` / `TranscodeManager` /
//! `SubtitleEncoder` / `AttachmentExtractor` traits. The actual process spawn
//! sits behind a `Transcoder` trait so unit tests use a fake. The full
//! hardware-acceleration matrix (nvenc/qsv/vaapi/videotoolbox) and BdInfo are
//! deferred; core software transcode + ffprobe parsing are ported.
//! Filled by the Wave 5 PortJob. See `brain/PLAN_HERMIT_PORT.md` + `brain/DEFERRED.md`.

pub mod attachments;
pub mod configuration;
pub mod encoder;
pub mod encoding_helper;
pub mod probing;
pub mod subtitles;
pub mod transcoding;

pub use attachments::{
    AttachmentExtractorImpl, AttachmentIo, MediaSourceResolver, NoopAttachmentIo,
};
pub use configuration::{
    DirChecker, EncodingConfigurationFactory, EncodingConfigurationStore, RealDirChecker,
};
pub use encoder::{MediaEncoderConfig, MediaEncoderImpl};
pub use encoding_helper::{
    BaseEncodingJobOptions, EncoderCapabilities, EncodingHelper, EncodingJobInfo,
    NoOptionalEncoders,
};
pub use transcoding::{
    HLS_PING_TIMEOUT_MS, NoopSessionReporter, PROGRESSIVE_PING_TIMEOUT_MS, SessionReporter,
    TranscodeManagerImpl,
};
