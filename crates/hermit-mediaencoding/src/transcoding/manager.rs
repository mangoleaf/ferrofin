//! Port of the job-registry half of `TranscodeManager`.

use std::sync::Mutex;

use async_trait::async_trait;
use hermit_traits::error::ServiceError;
use hermit_traits::media_encoding::{
    TranscodeManager, TranscodingJobHandle, TranscodingJobType, TranscodingProgress,
};

/// The kill-timer duration for a progressive stream, in milliseconds.
///
/// Port of the `timerDuration = 10000` default in `TranscodeManager.PingTimer`.
/// Progressive streams stop reliably on their own, so this is the short window.
pub const PROGRESSIVE_PING_TIMEOUT_MS: i64 = 10_000;

/// The kill-timer duration for a segmented (HLS/DASH) stream, in milliseconds.
///
/// Port of the `timerDuration = 60000` HLS branch in `TranscodeManager.PingTimer`.
pub const HLS_PING_TIMEOUT_MS: i64 = 60_000;

/// Receives progress reports and job teardown notifications.
///
/// Port of the `ISessionManager`/`ReportTranscodingProgress` call-outs and the
/// `DeletePartialStreamFiles` file cleanup; behind a seam because the session
/// layer and the filesystem are not part of this crate. All un-mockable
/// side effects (session messaging, deleting partial files) live here so the
/// registry stays pure and testable.
#[async_trait]
pub trait SessionReporter: Send + Sync {
    /// Reports `progress` for `job` to the session layer.
    ///
    /// Port of `ReportTranscodingProgress`.
    async fn report_progress(&self, job: &TranscodingJobHandle, progress: TranscodingProgress);

    /// Tears a killed `job` down, deleting its partial output when
    /// `delete_files` is set.
    ///
    /// Port of the `KillTranscodingJob` tail (`Stop` + `DeletePartialStreamFiles`).
    async fn on_job_killed(&self, job: &TranscodingJobHandle, delete_files: bool);
}

/// A [`SessionReporter`] that does nothing (for tests / progressive-only hosts).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSessionReporter;

#[async_trait]
impl SessionReporter for NoopSessionReporter {
    async fn report_progress(&self, _job: &TranscodingJobHandle, _progress: TranscodingProgress) {}
    async fn on_job_killed(&self, _job: &TranscodingJobHandle, _delete_files: bool) {}
}

/// A registered transcode job: its identifying [`TranscodingJobHandle`] plus the
/// mutable bookkeeping the registry tracks.
#[derive(Debug, Clone)]
struct RegisteredJob {
    handle: TranscodingJobHandle,
    is_user_paused: bool,
    ping_timeout_ms: i64,
    /// Number of active output requests (`ActiveRequestCount`); a job with zero
    /// begins its idle countdown toward kill.
    active_request_count: i32,
}

impl RegisteredJob {
    fn ping_timeout_for(job_type: TranscodingJobType) -> i64 {
        match job_type {
            TranscodingJobType::Progressive => PROGRESSIVE_PING_TIMEOUT_MS,
            TranscodingJobType::Hls | TranscodingJobType::Dash => HLS_PING_TIMEOUT_MS,
        }
    }
}

/// The `hermit-traits` [`TranscodeManager`] implementation (job registry only).
///
/// Generic over the [`SessionReporter`] seam. `StartFfMpeg` and session wiring
/// are deferred; this owns the active-job list and its lifecycle operations.
pub struct TranscodeManagerImpl<S: SessionReporter> {
    jobs: Mutex<Vec<RegisteredJob>>,
    reporter: S,
}

impl<S: SessionReporter> TranscodeManagerImpl<S> {
    /// Creates an empty registry reporting through `reporter`.
    pub fn new(reporter: S) -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            reporter,
        }
    }

    /// Registers a new job and returns its handle.
    ///
    /// Port of the `_activeTranscodingJobs.Add(job)` in `OnTranscodeBeginning`;
    /// exposed so the deferred `StartFfMpeg` implementation can enrol jobs.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub fn register_job(&self, handle: TranscodingJobHandle) -> TranscodingJobHandle {
        let job = RegisteredJob {
            ping_timeout_ms: RegisteredJob::ping_timeout_for(handle.job_type),
            is_user_paused: false,
            active_request_count: 1,
            handle: handle.clone(),
        };
        self.jobs.lock().expect("jobs lock poisoned").push(job);
        handle
    }

    /// Returns the number of currently registered jobs (test/inspection helper).
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn active_job_count(&self) -> usize {
        self.jobs.lock().expect("jobs lock poisoned").len()
    }

    /// Returns the ping timeout recorded for the job serving `play_session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn ping_timeout_for_session(&self, play_session_id: &str) -> Option<i64> {
        self.jobs
            .lock()
            .expect("jobs lock poisoned")
            .iter()
            .find(|j| {
                j.handle
                    .play_session_id
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(play_session_id))
            })
            .map(|j| j.ping_timeout_ms)
    }
}

#[async_trait]
impl<S: SessionReporter> TranscodeManager for TranscodeManagerImpl<S> {
    async fn get_transcoding_job_by_session(
        &self,
        play_session_id: &str,
    ) -> Result<Option<TranscodingJobHandle>, ServiceError> {
        let jobs = self.jobs.lock().expect("jobs lock poisoned");
        Ok(jobs
            .iter()
            .find(|j| {
                j.handle
                    .play_session_id
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(play_session_id))
            })
            .map(|j| j.handle.clone()))
    }

    async fn get_transcoding_job_by_path(
        &self,
        path: &str,
        job_type: TranscodingJobType,
    ) -> Result<Option<TranscodingJobHandle>, ServiceError> {
        let jobs = self.jobs.lock().expect("jobs lock poisoned");
        Ok(jobs
            .iter()
            .find(|j| j.handle.job_type == job_type && j.handle.path.eq_ignore_ascii_case(path))
            .map(|j| j.handle.clone()))
    }

    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError> {
        if play_session_id.trim().is_empty() {
            return Err(ServiceError::invalid_input("playSessionId is empty"));
        }
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        for job in jobs.iter_mut().filter(|j| {
            j.handle
                .play_session_id
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(play_session_id))
        }) {
            if let Some(paused) = is_user_paused {
                job.is_user_paused = paused;
            }
            // Refresh the kill-timer window for the job type (PingTimer).
            job.ping_timeout_ms = RegisteredJob::ping_timeout_for(job.handle.job_type);
        }
        Ok(())
    }

    async fn kill_transcoding_jobs(
        &self,
        device_id: &str,
        play_session_id: Option<&str>,
        delete_files: bool,
    ) -> Result<(), ServiceError> {
        let killed: Vec<TranscodingJobHandle> = {
            let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
            let mut killed = Vec::new();
            jobs.retain(|j| {
                let matches = match play_session_id {
                    Some(psid) if !psid.trim().is_empty() => j
                        .handle
                        .play_session_id
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(psid)),
                    _ => j
                        .handle
                        .device_id
                        .as_deref()
                        .is_some_and(|d| d.eq_ignore_ascii_case(device_id)),
                };
                if matches {
                    killed.push(j.handle.clone());
                }
                !matches
            });
            killed
        };

        for job in &killed {
            self.reporter.on_job_killed(job, delete_files).await;
        }
        Ok(())
    }

    async fn report_transcoding_progress(
        &self,
        job: &TranscodingJobHandle,
        progress: TranscodingProgress,
    ) -> Result<(), ServiceError> {
        self.reporter.report_progress(job, progress).await;
        Ok(())
    }

    async fn on_transcode_begin_request(
        &self,
        path: &str,
        job_type: TranscodingJobType,
    ) -> Result<Option<TranscodingJobHandle>, ServiceError> {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        let job = jobs
            .iter_mut()
            .find(|j| j.handle.job_type == job_type && j.handle.path.eq_ignore_ascii_case(path));
        Ok(job.map(|j| {
            // A new consumer arrived: bump the active-request count so the idle
            // kill timer does not fire (OnTranscodeBeginRequest).
            j.active_request_count += 1;
            j.handle.clone()
        }))
    }

    async fn on_transcode_end_request(
        &self,
        job: &TranscodingJobHandle,
    ) -> Result<(), ServiceError> {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        if let Some(registered) = jobs.iter_mut().find(|j| {
            j.handle.job_type == job.job_type && j.handle.path.eq_ignore_ascii_case(&job.path)
        }) {
            // Mirror OnTranscodeEndRequest decrementing ActiveRequestCount.
            registered.active_request_count = (registered.active_request_count - 1).max(0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hermit_traits::media_encoding::{
        TranscodeManager, TranscodingJobHandle, TranscodingJobType, TranscodingProgress,
    };

    use super::{
        HLS_PING_TIMEOUT_MS, NoopSessionReporter, PROGRESSIVE_PING_TIMEOUT_MS, SessionReporter,
        TranscodeManagerImpl,
    };

    fn handle(
        session: &str,
        path: &str,
        job_type: TranscodingJobType,
        device: &str,
    ) -> TranscodingJobHandle {
        TranscodingJobHandle {
            play_session_id: Some(session.to_owned()),
            path: path.to_owned(),
            job_type,
            device_id: Some(device.to_owned()),
        }
    }

    fn manager() -> TranscodeManagerImpl<NoopSessionReporter> {
        TranscodeManagerImpl::new(NoopSessionReporter)
    }

    #[tokio::test]
    async fn lookup_by_session_and_path() {
        let m = manager();
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        assert!(
            m.get_transcoding_job_by_session("s1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            m.get_transcoding_job_by_session("nope")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            m.get_transcoding_job_by_path("/t/a.m3u8", TranscodingJobType::Hls)
                .await
                .unwrap()
                .is_some()
        );
        // Wrong type does not match.
        assert!(
            m.get_transcoding_job_by_path("/t/a.m3u8", TranscodingJobType::Progressive)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ping_timeout_matches_job_type() {
        let m = manager();
        m.register_job(handle("hls", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        m.register_job(handle(
            "prog",
            "/t/b.mp4",
            TranscodingJobType::Progressive,
            "dev1",
        ));
        assert_eq!(m.ping_timeout_for_session("hls"), Some(HLS_PING_TIMEOUT_MS));
        assert_eq!(
            m.ping_timeout_for_session("prog"),
            Some(PROGRESSIVE_PING_TIMEOUT_MS)
        );
    }

    #[tokio::test]
    async fn ping_empty_session_is_invalid() {
        let m = manager();
        assert!(m.ping_transcoding_job("  ", None).await.is_err());
    }

    #[tokio::test]
    async fn kill_by_session_removes_only_that_job() {
        let m = manager();
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        m.register_job(handle("s2", "/t/b.m3u8", TranscodingJobType::Hls, "dev1"));
        m.kill_transcoding_jobs("dev1", Some("s1"), true)
            .await
            .unwrap();
        assert_eq!(m.active_job_count(), 1);
        assert!(
            m.get_transcoding_job_by_session("s2")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn kill_by_device_removes_all_device_jobs() {
        let m = manager();
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        m.register_job(handle("s2", "/t/b.m3u8", TranscodingJobType::Hls, "dev1"));
        m.register_job(handle("s3", "/t/c.m3u8", TranscodingJobType::Hls, "dev2"));
        m.kill_transcoding_jobs("dev1", None, false).await.unwrap();
        assert_eq!(m.active_job_count(), 1);
    }

    /// A reporter that records the kills it was asked to perform.
    struct RecordingReporter {
        killed: Mutex<Vec<(String, bool)>>,
    }

    #[async_trait]
    impl SessionReporter for RecordingReporter {
        async fn report_progress(
            &self,
            _job: &TranscodingJobHandle,
            _progress: TranscodingProgress,
        ) {
        }
        async fn on_job_killed(&self, job: &TranscodingJobHandle, delete_files: bool) {
            self.killed
                .lock()
                .unwrap()
                .push((job.path.clone(), delete_files));
        }
    }

    #[tokio::test]
    async fn kill_notifies_reporter_with_delete_flag() {
        let reporter = Arc::new(RecordingReporter {
            killed: Mutex::new(Vec::new()),
        });
        let m = TranscodeManagerImpl::new(ReporterHandle(Arc::clone(&reporter)));
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        m.kill_transcoding_jobs("dev1", Some("s1"), true)
            .await
            .unwrap();
        let killed = reporter.killed.lock().unwrap();
        assert_eq!(killed.len(), 1);
        assert_eq!(killed[0], ("/t/a.m3u8".to_owned(), true));
    }

    /// Adapter so an `Arc<RecordingReporter>` satisfies the `SessionReporter`
    /// bound by value.
    struct ReporterHandle(Arc<RecordingReporter>);

    #[async_trait]
    impl SessionReporter for ReporterHandle {
        async fn report_progress(&self, job: &TranscodingJobHandle, progress: TranscodingProgress) {
            self.0.report_progress(job, progress).await;
        }
        async fn on_job_killed(&self, job: &TranscodingJobHandle, delete_files: bool) {
            self.0.on_job_killed(job, delete_files).await;
        }
    }

    #[tokio::test]
    async fn begin_and_end_request_track_active_count() {
        let m = manager();
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        let begun = m
            .on_transcode_begin_request("/t/a.m3u8", TranscodingJobType::Hls)
            .await
            .unwrap();
        assert!(begun.is_some());
        let job = begun.unwrap();
        m.on_transcode_end_request(&job).await.unwrap();
        // Job is still registered (end just decrements the request count).
        assert_eq!(m.active_job_count(), 1);
    }

    #[tokio::test]
    async fn report_progress_is_ok() {
        let m = manager();
        let h = handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1");
        m.register_job(h.clone());
        assert!(
            m.report_transcoding_progress(&h, TranscodingProgress::default())
                .await
                .is_ok()
        );
    }
}
