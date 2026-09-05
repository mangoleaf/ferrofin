//! ffmpeg/ffprobe media encoding for Ferrofin — port of `MediaBrowser.MediaEncoding`
//! (+ the arg-building core of `EncodingHelper`).
//!
//! Implements the `ferrofin-traits` `MediaEncoder` / `TranscodeManager` /
//! `SubtitleEncoder` / `AttachmentExtractor` traits. The actual process spawn
//! sits behind a `Transcoder` trait so unit tests use a fake.
//!
//! Ported: the software transcode path, ffprobe parsing, subtitle/attachment
//! extraction, and the hardware environment probe ([`encoding_helper::hw`]).
//! The hardware argument builders themselves — device-init graphs, hardware
//! decoder selection, tonemapping, and the per-vendor filter chains — are the
//! named work items of the hardware-transcoding roadmap. Blu-ray (`BdInfo`) is
//! tracked separately as an open work item in that plan; it belongs to
//! disc-image playback rather than to encoding, so it needs its own plan.

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
    EncoderValidator, FfmpegVersion, MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder,
    Transcoder, TrickplayFrameExtractorImpl,
};
pub use encoding_helper::hw::{
    BsfOption, FfmpegCapabilities, FfmpegCapabilitiesBuilder, FilterOption, Platform,
    parse_os_release,
};
pub use encoding_helper::{
    BaseEncodingJobOptions, EncoderCapabilities, EncodingHelper, EncodingJobInfo,
    NoOptionalEncoders, TranscodeDisplayNames,
};
pub use subtitles::{SubtitleEncoder, SubtitleEncoderImpl, SubtitleIo};
pub use transcoding::{
    FakeScript, FakeSegmentTranscoder, FakeTranscodeChild, FileCleaner, FsFileCleaner,
    HLS_PING_TIMEOUT_MS, NoopSessionReporter, PROGRESSIVE_PING_TIMEOUT_MS,
    SEGMENT_READY_POLL_INTERVAL_MS, SegmentTranscoder, SessionReporter, SpawnRequest,
    TokioSegmentTranscoder, TranscodeChild, TranscodeManagerImpl, WAIT_FOR_FILE_TIMEOUT_MS,
};

pub use analysis::FfmpegMediaExtractor;
