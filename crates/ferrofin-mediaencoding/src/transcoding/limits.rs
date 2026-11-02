//! The admin caps on concurrent re-encoding jobs, and how a start they stop
//! is told apart from a start that failed.
//!
//! Jellyfin has no such caps: `TranscodeManager.StartFfMpeg` starts whatever
//! it is asked to, and `MaxActiveSessions` applies only at login. `DeviceId`
//! and `PlaySessionId` are the client's own strings, so one account can start
//! encode after encode and starve every other stream.

use std::ops::RangeInclusive;

use ferrofin_traits::error::ServiceError;
use thiserror::Error;

/// The seconds a client refused by a cap is told to wait before trying again,
/// its `Retry-After` (owner decision: a constant, not a setting).
pub const TRANSCODE_RETRY_AFTER_SECS: u32 = 10;

/// The caps on concurrent re-encoding jobs; `0` is off, the default for both.
///
/// A job counts when it re-encodes any stream
/// ([`EncodingJobInfo::encodes_any_stream`](crate::EncodingJobInfo::encodes_any_stream))
/// and its process has not exited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscodeLimits {
    /// Jobs the whole server may run (`max_transcodes`). A job from an API key
    /// counts here.
    pub server: u32,
    /// Jobs one user may run (`max_transcodes_per_user`). An API key has no
    /// user, so it is outside this cap.
    pub per_user: u32,
}

impl TranscodeLimits {
    /// The values each cap takes.
    pub const RANGE: RangeInclusive<u32> = 0..=1024;

    /// Whether `cap` leaves no room beside `running` jobs. A cap of `0` is off.
    pub(super) fn is_full(cap: u32, running: usize) -> bool {
        usize::try_from(cap).is_ok_and(|cap| cap > 0 && running >= cap)
    }
}

/// Which cap stopped a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TranscodeRefused {
    /// The user already runs as many jobs as `max_transcodes_per_user` allows.
    #[error("this user is running the most transcodes allowed; try again shortly")]
    User,
    /// The server already runs as many jobs as `max_transcodes` allows.
    #[error("the server is running the most transcodes allowed; try again shortly")]
    Server,
}

impl From<TranscodeRefused> for ServiceError {
    fn from(refused: TranscodeRefused) -> Self {
        Self::busy(refused.to_string(), TRANSCODE_RETRY_AFTER_SECS)
    }
}

/// Why [`start_ffmpeg`](super::TranscodeManagerImpl::start_ffmpeg) returned no
/// job.
#[derive(Debug, Error)]
pub enum StartFfmpegError {
    /// A cap stopped the job before anything started. The stream is fine and
    /// may play once a job ends, so this is no evidence of a bad source.
    #[error(transparent)]
    Refused(#[from] TranscodeRefused),
    /// The job could not start, or died starting.
    #[error("{0}")]
    Failed(String),
}

impl From<String> for StartFfmpegError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<StartFfmpegError> for ServiceError {
    fn from(error: StartFfmpegError) -> Self {
        match error {
            StartFfmpegError::Refused(refused) => refused.into(),
            StartFfmpegError::Failed(message) => {
                Self::backend(format!("failed to start transcode: {message}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StartFfmpegError, TRANSCODE_RETRY_AFTER_SECS, TranscodeLimits, TranscodeRefused};
    use ferrofin_traits::error::ServiceError;

    #[test]
    fn a_cap_is_full_at_its_value_and_zero_is_off() {
        assert!(!TranscodeLimits::is_full(0, 500));
        assert!(!TranscodeLimits::is_full(2, 1));
        assert!(TranscodeLimits::is_full(2, 2));
    }

    #[test]
    fn a_refusal_becomes_busy_and_a_failure_stays_a_backend_error() {
        let refused: ServiceError = StartFfmpegError::from(TranscodeRefused::Server).into();
        assert!(
            matches!(
                refused,
                ServiceError::Busy {
                    retry_after_secs: TRANSCODE_RETRY_AFTER_SECS,
                    ..
                }
            ),
            "{refused:?}"
        );
        let failed: ServiceError = StartFfmpegError::from("no input".to_owned()).into();
        assert!(matches!(failed, ServiceError::Backend(_)), "{failed:?}");
        assert_eq!(
            failed.to_string(),
            "backend error: failed to start transcode: no input"
        );
    }
}
