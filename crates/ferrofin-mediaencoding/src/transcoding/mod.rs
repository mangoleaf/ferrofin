//! Live transcode-job management.
//!
//! Port of the job-registry half of
//! `MediaBrowser.MediaEncoding.Transcoding.TranscodeManager`: the
//! `_activeTranscodingJobs` list and its lookup / ping / kill / begin / end
//! operations. The ffmpeg spawn + `StreamState`/session wiring (`StartFfMpeg`,
//! the throttler and segment cleaner) are **deferred** — this unit lands the
//! bookkeeping the [`TranscodeManager`](ferrofin_traits::media_encoding::TranscodeManager)
//! trait exposes. Progress reporting and job teardown call out to the
//! [`SessionReporter`] seam so unit tests inject a fake.

pub mod fs_wait;
pub mod manager;
pub mod segment_transcoder;
pub mod tokio_segment_transcoder;

pub use fs_wait::FsWaiter;
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
