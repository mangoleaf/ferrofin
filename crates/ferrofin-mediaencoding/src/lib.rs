//! ffmpeg/ffprobe media encoding for Ferrofin — port of `MediaBrowser.MediaEncoding`
//! (+ the arg-building core of `EncodingHelper`).
//!
//! Implements the `ferrofin-traits` `MediaEncoder` / `TranscodeManager` /
//! `SubtitleEncoder` / `AttachmentExtractor` traits. The actual process spawn
//! sits behind a `Transcoder` trait so unit tests use a fake. The full
//! hardware-acceleration matrix (nvenc/qsv/vaapi/videotoolbox) and BdInfo are
//! deferred; core software transcode + ffprobe parsing are ported.

pub mod analysis;
pub mod attachments;
pub mod configuration;
pub mod encoder;
pub mod encoding_helper;
pub mod error;
pub mod keyed_locks;
pub mod probing;
pub mod subtitles;
pub mod transcoding;

pub use error::MediaEncodingError;

pub use attachments::{
    AttachmentExtractorImpl, AttachmentIo, MediaSourceResolver, NoopAttachmentIo,
};
pub use configuration::{
    DirChecker, EncodingConfigurationFactory, EncodingConfigurationStore, RealDirChecker,
};
pub use encoder::{
    MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder, Transcoder, TrickplayFrameExtractorImpl,
};
pub use encoding_helper::{
    BaseEncodingJobOptions, EncoderCapabilities, EncodingHelper, EncodingJobInfo,
    NoOptionalEncoders, ProbedEncoders, TranscodeDisplayNames,
};
pub use subtitles::{SubtitleEncoder, SubtitleEncoderImpl, SubtitleIo};
pub use transcoding::{
    FakeScript, FakeSegmentTranscoder, FakeTranscodeChild, FileCleaner, FsFileCleaner,
    HLS_PING_TIMEOUT_MS, NoopSessionReporter, PROGRESSIVE_PING_TIMEOUT_MS,
    SEGMENT_READY_POLL_INTERVAL_MS, SegmentTranscoder, SessionReporter, SpawnRequest,
    TokioSegmentTranscoder, TranscodeChild, TranscodeManagerImpl, WAIT_FOR_FILE_TIMEOUT_MS,
};

pub use analysis::FfmpegMediaExtractor;
