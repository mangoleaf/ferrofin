//! Live transcode-job management.
//!
//! Port of the job-registry half of
//! `MediaBrowser.MediaEncoding.Transcoding.TranscodeManager`: the
//! `_activeTranscodingJobs` list and its lookup / ping / kill / begin / end
//! operations. The ffmpeg spawn + `StreamState`/session wiring (`StartFfMpeg`,
//! the throttler and segment cleaner) are **deferred** — this unit lands the
//! bookkeeping the [`TranscodeManager`](hermit_traits::media_encoding::TranscodeManager)
//! trait exposes. Progress reporting and job teardown call out to the
//! [`SessionReporter`] seam so unit tests inject a fake.

pub mod manager;

pub use manager::{
    HLS_PING_TIMEOUT_MS, NoopSessionReporter, PROGRESSIVE_PING_TIMEOUT_MS, SessionReporter,
    TranscodeManagerImpl,
};
