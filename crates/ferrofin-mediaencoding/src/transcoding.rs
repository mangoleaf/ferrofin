//! Live transcode registry, ffmpeg startup, progress and runtime controls.

mod ffmpeg_log;
pub mod fs_wait;
pub mod limits;
pub mod manager;
pub mod progress;
pub mod segment_cleaner;
pub mod segment_transcoder;
pub mod throttler;
pub mod tokio_segment_transcoder;

pub(crate) use ffmpeg_log::shown_argument_line;
pub use fs_wait::FsWaiter;
pub use limits::{StartFfmpegError, TRANSCODE_RETRY_AFTER_SECS, TranscodeLimits, TranscodeRefused};
pub use manager::{
    FileCleaner, FsFileCleaner, HLS_PING_TIMEOUT_MS, NoopSessionReporter,
    PROGRESSIVE_PING_TIMEOUT_MS, SEGMENT_READY_POLL_INTERVAL_MS, SessionReporter,
    TranscodeManagerImpl, WAIT_FOR_FILE_TIMEOUT_MS,
};
pub use segment_transcoder::{
    FakeScript, FakeSegmentTranscoder, FakeTranscodeChild, SegmentTranscoder, SpawnRequest,
    TranscodeChild,
};
pub use tokio_segment_transcoder::TokioSegmentTranscoder;
