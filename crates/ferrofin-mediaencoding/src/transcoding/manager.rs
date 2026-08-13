//! Port of the job-registry half of `TranscodeManager`, plus the `StartFfMpeg`
//! spawn orchestration.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{
    TranscodeManager, TranscodingJobHandle, TranscodingJobType, TranscodingProgress,
};

use crate::encoding_helper::EncodingJobInfo;

use super::segment_transcoder::{SegmentTranscoder, SpawnRequest, TranscodeChild};

/// How long `start_ffmpeg` blocks for the first segment before erroring, in
/// milliseconds.
///
/// The C# `StartFfMpeg` loops unbounded on the cancellation token; this port
/// adds a bound so a wedged ffmpeg cannot hang the request forever. Configurable
/// candidate (default 180 s).
///
/// Sized for the worst legitimate cold start: a software 4K HEVC→H.264 encode
/// of one 6 s segment on a small CPU cap (~4 cores) takes 30–60 s — the old
/// 30 s bound killed those jobs mid-encode, and because the timeout also
/// deletes the job, client retries restarted from zero and playback never
/// began. Jellyfin waits as long as the request stays open.
pub const WAIT_FOR_FILE_TIMEOUT_MS: u64 = 180_000;

/// The poll cadence for the "wait until the segment file exists" loops, in
/// milliseconds. Port of the hard-coded `Task.Delay(100)` in `StartFfMpeg` and
/// `GetSegmentResult`.
pub const SEGMENT_READY_POLL_INTERVAL_MS: u64 = 100;

/// The inputs to [`TranscodeManagerImpl::start_ffmpeg`].
///
/// Bundles the `StartFfMpeg` arguments (`outputPath`, `commandLineArguments`,
/// `workingDirectory`, the log target) into one value so the call stays under
/// the argument-count lint and reads as a request.
#[derive(Debug, Clone)]
pub struct StartFfMpegRequest<'a> {
    /// The resolved ffmpeg binary path (`MediaEncoder::encoder_path`).
    pub program: &'a str,
    /// The job state (`StreamState`) — supplies wait-for-path, session/device
    /// ids, transcode type, and the segment container.
    pub state: &'a EncodingJobInfo,
    /// The playlist/output file path ffmpeg writes (`outputPath`).
    pub output_path: &'a Path,
    /// The fully-built ffmpeg arguments (`commandLineArguments`).
    pub arguments: Vec<String>,
    /// The stderr log target for this transcode (`FFmpeg.Transcode-*.log`).
    pub log_path: PathBuf,
    /// The process working directory, if any (`workingDirectory`).
    pub working_dir: Option<PathBuf>,
}

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

/// Deletes a killed HLS job's partial segment files from its cache directory.
///
/// Port of `DeleteHlsPartialStreamFiles`: every file in `output_dir` whose name
/// contains the playlist stem is removed. Behind a seam so kill/cleanup stays
/// unit-testable against a temp dir without touching a real transcode cache.
pub trait FileCleaner: Send + Sync {
    /// Deletes every file in `output_dir` whose filename contains
    /// `playlist_stem` (case-insensitively). Port of `DeleteHlsPartialStreamFiles`.
    fn delete_partial_stream_files(&self, output_dir: &Path, playlist_stem: &str);
}

/// The real [`FileCleaner`]: deletes files with `std::fs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsFileCleaner;

impl FileCleaner for FsFileCleaner {
    fn delete_partial_stream_files(&self, output_dir: &Path, playlist_stem: &str) {
        let Ok(entries) = std::fs::read_dir(output_dir) else {
            return;
        };
        let stem_lower = playlist_stem.to_ascii_lowercase();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(&stem_lower)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The live half of a job spawned by `start_ffmpeg`: the process handle plus the
/// cache-directory metadata kill/cleanup needs.
///
/// Owned only inside the impl (never exposed through `ferrofin-traits`). Port of
/// the ffmpeg-`Process` + output-path members of the C# `TranscodingJob`.
struct RunningJob {
    /// The live process handle.
    child: Box<dyn TranscodeChild>,
    /// The segment cache directory to purge on kill (`DeleteHlsPartialStreamFiles`).
    output_dir: PathBuf,
    /// The `.m3u8`/output path whose stem selects the files to delete.
    playlist_path: PathBuf,
    /// The segment file extension (e.g. `.ts`), for diagnostics/index scans.
    #[allow(dead_code)]
    segment_extension: String,
}

/// A registered transcode job: its identifying [`TranscodingJobHandle`] plus the
/// mutable bookkeeping the registry tracks.
struct RegisteredJob {
    handle: TranscodingJobHandle,
    is_user_paused: bool,
    ping_timeout_ms: i64,
    /// Number of active output requests (`ActiveRequestCount`); a job with zero
    /// begins its idle countdown toward kill.
    active_request_count: i32,
    /// When this job last showed signs of a consumer: registration, a
    /// begin/end request, or a session ping. The idle reaper kills a job with
    /// no active requests once this is older than [`Self::ping_timeout_ms`].
    last_activity: std::time::Instant,
    /// The live process + cache metadata when this job was spawned via
    /// `start_ffmpeg`; `None` for registry-only jobs (lookup tests).
    running: Option<RunningJob>,
}

impl RegisteredJob {
    fn ping_timeout_for(job_type: TranscodingJobType) -> i64 {
        match job_type {
            TranscodingJobType::Progressive => PROGRESSIVE_PING_TIMEOUT_MS,
            TranscodingJobType::Hls | TranscodingJobType::Dash => HLS_PING_TIMEOUT_MS,
        }
    }
}

/// The `ferrofin-traits` [`TranscodeManager`] implementation plus `StartFfMpeg`.
///
/// Generic over the [`SessionReporter`] and [`FileCleaner`] seams. Owns the
/// active-job list, spawns transcodes through a [`SegmentTranscoder`], and tears
/// them down on kill.
pub struct TranscodeManagerImpl<S: SessionReporter, C: FileCleaner = FsFileCleaner> {
    jobs: Mutex<Vec<RegisteredJob>>,
    reporter: S,
    file_cleaner: C,
}

/// An active-consumer mark on a transcode job, released on drop.
///
/// Returned by [`TranscodeManagerImpl::begin_request_guard`]; holding it keeps
/// the idle reaper away from the job. Dropping it (including when the awaiting
/// request future is cancelled by a client disconnect) decrements the count and
/// restarts the idle countdown.
pub struct TranscodeRequestGuard<'a, S: SessionReporter, C: FileCleaner> {
    manager: &'a TranscodeManagerImpl<S, C>,
    handle: TranscodingJobHandle,
}

impl<S: SessionReporter, C: FileCleaner> Drop for TranscodeRequestGuard<'_, S, C> {
    fn drop(&mut self) {
        self.manager.end_request_sync(&self.handle);
    }
}

impl<S: SessionReporter> TranscodeManagerImpl<S, FsFileCleaner> {
    /// Creates an empty registry reporting through `reporter`, deleting partial
    /// files with the real [`FsFileCleaner`].
    pub fn new(reporter: S) -> Self {
        Self::with_file_cleaner(reporter, FsFileCleaner)
    }
}

impl<S: SessionReporter, C: FileCleaner> TranscodeManagerImpl<S, C> {
    /// Creates an empty registry with an explicit `file_cleaner` seam (tests
    /// inject a recording cleaner over a temp dir).
    pub fn with_file_cleaner(reporter: S, file_cleaner: C) -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            reporter,
            file_cleaner,
        }
    }

    /// Registers a new job and returns its handle.
    ///
    /// Port of the `_activeTranscodingJobs.Add(job)` in `OnTranscodeBeginning`;
    /// exposed so `start_ffmpeg` (and callers) can enrol jobs.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub fn register_job(&self, handle: TranscodingJobHandle) -> TranscodingJobHandle {
        let job = RegisteredJob {
            ping_timeout_ms: RegisteredJob::ping_timeout_for(handle.job_type),
            is_user_paused: false,
            // Consumers are counted by begin/end request pairing (the guard);
            // registration itself stamps last_activity, which keeps the idle
            // reaper away until the initiating request attaches its guard.
            active_request_count: 0,
            last_activity: std::time::Instant::now(),
            handle: handle.clone(),
            running: None,
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

    /// Spawns an ffmpeg transcode and blocks until its first segment exists.
    ///
    /// Port of `TranscodeManager.StartFfMpeg`: create the output directory,
    /// register the job *first* (so a concurrent segment request finds it),
    /// spawn through the [`SegmentTranscoder`] seam, then poll until the target
    /// file exists or the process exits — surfacing a non-zero exit as an error.
    /// The `AcquireResources` / attachment-extraction / user-permission steps and
    /// the throttler / segment-cleaner tasks are deferred (software path only).
    ///
    /// # Errors
    ///
    /// Returns an error if the output directory cannot be created, the process
    /// cannot be spawned, ffmpeg exits non-zero, or the first segment does not
    /// appear within [`WAIT_FOR_FILE_TIMEOUT_MS`].
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[tracing::instrument(
        name = "transcode_job",
        skip_all,
        fields(play_session_id = tracing::field::Empty, device_id = tracing::field::Empty)
    )]
    pub async fn start_ffmpeg(
        &self,
        transcoder: &dyn SegmentTranscoder,
        request: StartFfMpegRequest<'_>,
    ) -> Result<TranscodingJobHandle, String> {
        let StartFfMpegRequest {
            program,
            state,
            output_path,
            arguments,
            log_path,
            working_dir,
        } = request;

        // Identify the job on its span (inherited by every event below) and log
        // the start; the full ffmpeg arg vector is diagnostic but verbose → debug.
        let job_span = tracing::Span::current();
        job_span.record(
            "play_session_id",
            state.play_session_id.as_deref().unwrap_or(""),
        );
        job_span.record("device_id", state.device_id.as_deref().unwrap_or(""));
        tracing::info!(
            transcode_type = ?state.transcoding_type,
            program = %program,
            args = arguments.len(),
            "transcode job starting"
        );
        tracing::debug!(ffmpeg_args = ?arguments, "ffmpeg arguments");

        // 1. Directory.CreateDirectory(Path.GetDirectoryName(outputPath)).
        let directory = output_path
            .parent()
            .ok_or_else(|| format!("output path {} has no parent", output_path.display()))?
            .to_path_buf();
        std::fs::create_dir_all(&directory)
            .map_err(|e| format!("create output dir {}: {e}", directory.display()))?;

        // 3. Build the spawn request (steps 2 = Acquire/attachment deferred).
        let stderr_log = log_path.clone();
        let req = SpawnRequest {
            program: program.to_owned(),
            arguments,
            working_dir,
            output_dir: directory.clone(),
            log_path,
        };

        let segment_extension = segment_file_extension(state.segment_container.as_deref());
        let handle = TranscodingJobHandle {
            play_session_id: state.play_session_id.clone(),
            path: output_path.to_string_lossy().into_owned(),
            job_type: state.transcoding_type,
            device_id: state.device_id.clone(),
        };

        // 4. Register the job FIRST (OnTranscodeBeginning).
        self.register_job(handle.clone());

        // 5. Spawn; on failure remove the job (OnTranscodeFailedToStart).
        let child = match transcoder.start_transcode(&req).await {
            Ok(child) => child,
            Err(e) => {
                self.remove_job_by_path(&handle.path, handle.job_type);
                return Err(e);
            }
        };

        // 6. Store the live job so kill/cleanup can reach the child + cache dir.
        self.attach_running(
            &handle,
            RunningJob {
                child,
                output_dir: directory,
                playlist_path: output_path.to_path_buf(),
                segment_extension,
            },
        );

        // 7. Wait until the target file exists OR the process exits, bounded.
        let target = state
            .wait_for_path
            .clone()
            .unwrap_or_else(|| output_path.to_path_buf());
        let deadline_polls = WAIT_FOR_FILE_TIMEOUT_MS / SEGMENT_READY_POLL_INTERVAL_MS.max(1);
        let mut polls = 0u64;
        loop {
            let exited = self.with_running(&handle, |r| r.child.has_exited());
            if target.exists() || exited == Some(true) {
                break;
            }
            polls += 1;
            if polls > deadline_polls {
                self.kill_and_remove(&handle, true).await;
                return Err(format!(
                    "timed out waiting for {} after {WAIT_FOR_FILE_TIMEOUT_MS}ms",
                    target.display()
                ));
            }
            tokio::time::sleep(Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS)).await;
        }

        // 8. A finished job that failed is an error (mirror the FfmpegException).
        self.fail_if_exited_nonzero(&handle, &stderr_log)?;

        // 9. Throttler / segment-cleaner deferred. 10. Return the handle.
        Ok(handle)
    }

    /// After the first-segment wait, turns an already-exited-nonzero ffmpeg into
    /// an error (and logs the silent death); a clean early exit logs at debug.
    fn fail_if_exited_nonzero(
        &self,
        handle: &TranscodingJobHandle,
        stderr_log: &Path,
    ) -> Result<(), String> {
        if self.with_running(handle, |r| r.child.has_exited()) != Some(true) {
            return Ok(());
        }
        let code = self.with_running(handle, |r| r.child.exit_code()).flatten();
        if code.unwrap_or(0) != 0 {
            self.remove_job_by_path(&handle.path, handle.job_type);
            // The bare exit code hides the actual cause (unreadable input, bad
            // args); ffmpeg's stderr in the transcode log names it.
            let tail = stderr_log_tail(stderr_log);
            // A silent ffmpeg death is the #1 playback-debug pain: log the exit +
            // where the full stderr lives. `warn!` (not `error!`) — the error
            // propagates and is logged once at the request boundary.
            tracing::warn!(
                exit_code = code.unwrap_or(-1),
                log = %stderr_log.display(),
                "ffmpeg exited non-zero during transcode startup"
            );
            return Err(format!(
                "FFmpeg exited with code {}{tail}",
                code.unwrap_or(-1)
            ));
        }
        tracing::debug!("ffmpeg exited 0 before the first segment appeared");
        Ok(())
    }

    /// Waits until segment `index` is ready to serve for `playlist_path`.
    ///
    /// The transcode runs with `-hls_flags temp_file`, so a segment file appears
    /// atomically complete (ffmpeg writes a `.tmp` and renames it only when the
    /// segment is fully written). So — unlike Jellyfin's `GetSegmentResult`, which
    /// waits for segment `index + 1` to prove `index` is done — the segment is
    /// ready the moment its own file exists. Dropping the `+1` wait roughly halves
    /// time-to-first-segment (~4.3s → ~2.4s here) on start and on every seek.
    ///
    /// Polls every [`SEGMENT_READY_POLL_INTERVAL_MS`] until the file exists or the
    /// job exits; returns `true` if the segment ends up on disk.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub async fn wait_for_segment(
        &self,
        handle: &TranscodingJobHandle,
        playlist_path: &Path,
        index: i32,
    ) -> bool {
        let ext = self
            .with_running(handle, |r| r.segment_extension.clone())
            .unwrap_or_else(|| ".ts".to_owned());
        let seg = segment_path(playlist_path, index, &ext);

        loop {
            if seg.exists() {
                return true;
            }
            let exited = self
                .with_running(handle, |r| r.child.has_exited())
                .unwrap_or(true);
            if exited {
                if seg.exists() {
                    return true;
                }
                // The job is gone and the segment never appeared. An ffmpeg
                // that exits *cleanly* without reaching the target — a
                // truncated/unreadable source on flaky network storage, or a
                // seek past the file's real end — is otherwise invisible at
                // WARN: the client just sees failed segment requests. Name it
                // here, with the stderr tail, so the operator has a trail.
                let code = self.with_running(handle, |r| r.child.exit_code()).flatten();
                let log = playlist_path.with_extension("log");
                tracing::warn!(
                    segment = index,
                    exit_code = code.unwrap_or(-1),
                    log = %log.display(),
                    "transcode exited before producing segment{}",
                    stderr_log_tail(&log)
                );
                return false;
            }
            tokio::time::sleep(Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS)).await;
        }
    }

    /// Kills the process behind `handle` (if any) and removes it, deleting its
    /// partial segment files when `delete_files` is set. Shared by the timeout
    /// path, the kill-timer, and `kill_transcoding_jobs`.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub async fn kill_and_remove(&self, handle: &TranscodingJobHandle, delete_files: bool) {
        let running = {
            let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
            let idx = jobs.iter().position(|j| {
                j.handle.job_type == handle.job_type
                    && j.handle.path.eq_ignore_ascii_case(&handle.path)
            });
            idx.and_then(|i| jobs.remove(i).running)
        };
        if let Some(running) = running {
            tracing::info!(
                play_session_id = handle.play_session_id.as_deref().unwrap_or(""),
                delete_files,
                "transcode job killed"
            );
            let _ = running.child.kill().await;
            if delete_files {
                let stem = running
                    .playlist_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.file_cleaner
                    .delete_partial_stream_files(&running.output_dir, &stem);
            }
        }
        self.reporter.on_job_killed(handle, delete_files).await;
    }

    /// One sweep of the idle reaper: kills every job that has **no active
    /// consumer** (`active_request_count == 0`) and no activity (request or
    /// session ping) within its ping timeout. Returns the killed handles.
    ///
    /// Port of upstream's per-job `PingTimer` → `OnTranscodeKillTimerStopped`
    /// collapsed into a periodic scan — same outcome (an HLS job dies ~60s
    /// after its last consumer vanishes, a progressive one after ~10s), one
    /// mechanism instead of a timer per job. This is what stops a cast client
    /// that disconnects without a Stop report from leaving ffmpeg running
    /// forever.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub async fn reap_idle_jobs(&self) -> Vec<TranscodingJobHandle> {
        let idle: Vec<TranscodingJobHandle> = {
            let jobs = self.jobs.lock().expect("jobs lock poisoned");
            jobs.iter()
                .filter(|j| {
                    j.active_request_count <= 0
                        && i64::try_from(j.last_activity.elapsed().as_millis()).unwrap_or(i64::MAX)
                            > j.ping_timeout_ms
                })
                .map(|j| j.handle.clone())
                .collect()
        };
        for handle in &idle {
            tracing::info!(
                path = %handle.path,
                play_session_id = handle.play_session_id.as_deref().unwrap_or(""),
                device_id = handle.device_id.as_deref().unwrap_or(""),
                "killing idle transcode job (no consumer within the ping timeout)"
            );
            self.kill_and_remove(handle, true).await;
        }
        idle
    }

    /// Runs [`Self::reap_idle_jobs`] forever, sweeping every `interval`. The
    /// composition root spawns this once next to the manager.
    pub async fn run_idle_reaper(self: std::sync::Arc<Self>, interval: Duration) {
        loop {
            tokio::time::sleep(interval).await;
            self.reap_idle_jobs().await;
        }
    }

    /// Marks a consumer active on the job registered for `path`/`job_type`,
    /// returning a guard that releases it on drop.
    ///
    /// The guard pairing is what upstream does with `OnTranscodeBeginRequest` /
    /// `Response.OnCompleted → OnTranscodeEndRequest`, made cancellation-safe:
    /// a segment wait aborted by a client disconnect still decrements, so the
    /// idle reaper's `active_request_count == 0` check stays truthful.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn begin_request_guard(
        &self,
        path: &str,
        job_type: TranscodingJobType,
    ) -> Option<TranscodeRequestGuard<'_, S, C>> {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        let job = jobs
            .iter_mut()
            .find(|j| j.handle.job_type == job_type && j.handle.path.eq_ignore_ascii_case(path))?;
        job.active_request_count += 1;
        job.last_activity = std::time::Instant::now();
        let handle = job.handle.clone();
        drop(jobs);
        Some(TranscodeRequestGuard {
            manager: self,
            handle,
        })
    }

    /// Synchronous end-request: decrements the job's active-consumer count and
    /// refreshes its activity stamp (the idle countdown starts *now*).
    fn end_request_sync(&self, handle: &TranscodingJobHandle) {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        if let Some(job) = jobs.iter_mut().find(|j| {
            j.handle.job_type == handle.job_type && j.handle.path.eq_ignore_ascii_case(&handle.path)
        }) {
            job.active_request_count = (job.active_request_count - 1).max(0);
            job.last_activity = std::time::Instant::now();
        }
    }

    /// Removes the registered job for `path`/`job_type` (no child teardown).
    fn remove_job_by_path(&self, path: &str, job_type: TranscodingJobType) {
        self.jobs.lock().expect("jobs lock poisoned").retain(|j| {
            !(j.handle.job_type == job_type && j.handle.path.eq_ignore_ascii_case(path))
        });
    }

    /// Attaches the live [`RunningJob`] to the registered entry for `handle`.
    fn attach_running(&self, handle: &TranscodingJobHandle, running: RunningJob) {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        if let Some(job) = jobs.iter_mut().find(|j| {
            j.handle.job_type == handle.job_type && j.handle.path.eq_ignore_ascii_case(&handle.path)
        }) {
            job.running = Some(running);
        }
    }

    /// Reads the [`RunningJob`] for `handle` under the lock, mapping it with `f`.
    fn with_running<R>(
        &self,
        handle: &TranscodingJobHandle,
        f: impl FnOnce(&RunningJob) -> R,
    ) -> Option<R> {
        let jobs = self.jobs.lock().expect("jobs lock poisoned");
        jobs.iter()
            .find(|j| {
                j.handle.job_type == handle.job_type
                    && j.handle.path.eq_ignore_ascii_case(&handle.path)
            })
            .and_then(|j| j.running.as_ref())
            .map(f)
    }
}

/// The segment file extension for `segment_container` (`.ts` default).
///
/// Port of `EncodingHelper.GetSegmentFileExtension` (mirrors the `ferrofin-hls`
/// helper) so the cache-dir naming here agrees with the playlist generator.
fn segment_file_extension(segment_container: Option<&str>) -> String {
    match segment_container.map(str::trim).filter(|s| !s.is_empty()) {
        Some("mp4") => ".mp4".to_owned(),
        _ => ".ts".to_owned(),
    }
}

/// The last few lines of ffmpeg's stderr log, formatted for appending to the
/// exit-code error (empty when the log is absent or empty).
///
/// A failed transcode's bare exit code says nothing; stderr names the cause
/// (unreadable input, bad arguments). The log is small at start-failure — the
/// process died before producing output.
fn stderr_log_tail(log_path: &Path) -> String {
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines
        .iter()
        .rev()
        .take(4)
        .rev()
        .copied()
        .collect::<Vec<_>>();
    if tail.is_empty() {
        String::new()
    } else {
        format!("; stderr tail: {}", tail.join(" | "))
    }
}

/// The on-disk path of segment `index` for `playlist`.
///
/// Port of `GetSegmentPath`: `<folder>/<playlist-stem><index><ext>`.
fn segment_path(playlist: &Path, index: i32, extension: &str) -> PathBuf {
    let folder = playlist.parent().unwrap_or_else(|| Path::new(""));
    let stem = playlist
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    folder.join(format!("{stem}{index}{extension}"))
}

#[async_trait]
impl<S: SessionReporter, C: FileCleaner> TranscodeManager for TranscodeManagerImpl<S, C> {
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
            // Refresh the kill-timer window for the job type (PingTimer): the
            // idle countdown restarts from this ping.
            job.ping_timeout_ms = RegisteredJob::ping_timeout_for(job.handle.job_type);
            job.last_activity = std::time::Instant::now();
        }
        Ok(())
    }

    async fn kill_transcoding_jobs(
        &self,
        device_id: &str,
        play_session_id: Option<&str>,
        delete_files: bool,
    ) -> Result<(), ServiceError> {
        // Collect the matching handles under the lock, then tear each down via
        // `kill_and_remove` (child.kill + partial-file delete + reporter) so a
        // live spawned job's process is actually stopped, not just dropped.
        let killed: Vec<TranscodingJobHandle> = {
            let jobs = self.jobs.lock().expect("jobs lock poisoned");
            jobs.iter()
                .filter(|j| match play_session_id {
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
                })
                .map(|j| j.handle.clone())
                .collect()
        };

        for job in &killed {
            self.kill_and_remove(job, delete_files).await;
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
            // reaper does not fire (OnTranscodeBeginRequest).
            j.active_request_count += 1;
            j.last_activity = std::time::Instant::now();
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
            // Mirror OnTranscodeEndRequest decrementing ActiveRequestCount; the
            // idle countdown starts from this moment.
            registered.active_request_count = (registered.active_request_count - 1).max(0);
            registered.last_activity = std::time::Instant::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ferrofin_traits::media_encoding::{
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

/// Tests for the `start_ffmpeg` spawn orchestration, driven by the
/// [`FakeSegmentTranscoder`] seam so no real ffmpeg is involved.
#[cfg(test)]
mod start_ffmpeg_tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;

    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_traits::media_encoding::{TranscodeManager, TranscodingJobType};

    use crate::encoding_helper::{BaseEncodingJobOptions, EncodingJobInfo};
    use crate::transcoding::segment_transcoder::{FakeScript, FakeSegmentTranscoder};

    use super::{
        FileCleaner, FsFileCleaner, NoopSessionReporter, StartFfMpegRequest, TranscodeManagerImpl,
    };

    /// Builds a `StartFfMpegRequest` for `state`/`output_path` with `args`, its
    /// log alongside the playlist.
    fn ffmpeg_req<'a>(
        state: &'a EncodingJobInfo,
        output_path: &'a Path,
        args: Vec<String>,
    ) -> StartFfMpegRequest<'a> {
        StartFfMpegRequest {
            program: "ffmpeg",
            state,
            output_path,
            arguments: args,
            log_path: output_path.with_extension("log"),
            working_dir: None,
        }
    }

    /// Builds an HLS `EncodingJobInfo` writing its playlist at `output_path`.
    fn state(output_path: &Path, wait_for: Option<&Path>) -> EncodingJobInfo {
        EncodingJobInfo {
            base_request: BaseEncodingJobOptions::default(),
            video_stream: None,
            audio_stream: None,
            subtitle_stream: None,
            media_source: MediaSourceInfo::default(),
            output_video_codec: None,
            output_audio_codec: None,
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: None,
            output_video_sync: None,
            output_file_path: output_path.to_string_lossy().into_owned(),
            input_container: None,
            is_input_video: true,
            subtitle_delivery_method: ferrofin_model::dlna::SubtitleDeliveryMethod::Encode,
            run_time_ticks: None,
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            segment_length_secs: 6,
            wait_for_path: wait_for.map(Path::to_path_buf),
            segment_container: Some("ts".to_owned()),
            play_session_id: Some("sess".to_owned()),
            device_id: Some("dev".to_owned()),
        }
    }

    fn manager() -> TranscodeManagerImpl<NoopSessionReporter> {
        TranscodeManagerImpl::new(NoopSessionReporter)
    }

    #[tokio::test]
    async fn start_ffmpeg_creates_dir_registers_job_and_waits_for_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("session");
        let playlist = out_dir.join("out.m3u8");
        // Fake writes the playlist (the wait target) synchronously on spawn.
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));

        let handle = m
            .start_ffmpeg(
                &fake,
                ffmpeg_req(&st, &playlist, vec!["-i".to_owned(), "in.mkv".to_owned()]),
            )
            .await
            .expect("start_ffmpeg");

        assert!(out_dir.exists(), "output dir created");
        assert!(playlist.exists(), "wait target created");
        assert_eq!(m.active_job_count(), 1);
        assert_eq!(handle.job_type, TranscodingJobType::Hls);
        // The seam received the fully-built args.
        let reqs = fake.requests.lock().unwrap();
        assert_eq!(
            reqs[0].arguments,
            vec!["-i".to_owned(), "in.mkv".to_owned()]
        );
        assert_eq!(reqs[0].output_dir, out_dir);
    }

    #[tokio::test]
    async fn start_ffmpeg_errors_when_seam_spawn_fails() {
        struct FailingSeam;
        #[async_trait::async_trait]
        impl crate::transcoding::segment_transcoder::SegmentTranscoder for FailingSeam {
            async fn start_transcode(
                &self,
                _req: &crate::transcoding::segment_transcoder::SpawnRequest,
            ) -> Result<Box<dyn crate::transcoding::segment_transcoder::TranscodeChild>, String>
            {
                Err("boom".to_owned())
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("s").join("out.m3u8");
        let m = manager();
        let st = state(&playlist, None);
        let err = m
            .start_ffmpeg(&FailingSeam, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap_err();
        assert_eq!(err, "boom");
        // Failed start removes the job (OnTranscodeFailedToStart).
        assert_eq!(m.active_job_count(), 0);
    }

    #[tokio::test]
    async fn start_ffmpeg_surfaces_nonzero_exit_as_error() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        // Child exits immediately with a non-zero code; the wait target is
        // written so the loop breaks on exit, then the exit code is checked.
        let fake = FakeSegmentTranscoder::new(FakeScript {
            extra_files: vec!["out.m3u8".to_owned()],
            exit_code: 1,
            exits_immediately: true,
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let err = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap_err();
        assert!(err.contains("exited with code 1"), "got: {err}");
        assert_eq!(m.active_job_count(), 0);
    }

    #[tokio::test]
    async fn wait_for_segment_ready_when_next_segment_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        // Fake writes out0.ts, out1.ts and the playlist up front: segment 0 is
        // ready because segment 1 (next) exists.
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned(), "out1.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let handle = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();
        assert!(m.wait_for_segment(&handle, &playlist, 0).await);
    }

    #[tokio::test]
    async fn wait_for_segment_ready_when_job_has_exited() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let handle = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();
        // Only segment 0 exists (no next), but the job exited → ready.
        assert!(m.wait_for_segment(&handle, &playlist, 0).await);
    }

    #[tokio::test]
    async fn wait_for_segment_false_when_job_died_without_producing_it() {
        // The ffmpeg exits cleanly but never writes the awaited segment (a
        // truncated/unreadable source): the wait must report not-ready (and
        // logs the exit + stderr tail) instead of spinning.
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            extra_files: vec!["out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let handle = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();
        assert!(!m.wait_for_segment(&handle, &playlist, 3).await);
    }

    /// A [`FileCleaner`] that records the deletions it was asked to perform, and
    /// still performs them, so kill/cleanup is observable over a temp dir.
    #[derive(Clone)]
    struct RecordingCleaner {
        calls: Arc<Mutex<Vec<(std::path::PathBuf, String)>>>,
    }

    impl FileCleaner for RecordingCleaner {
        fn delete_partial_stream_files(&self, output_dir: &Path, playlist_stem: &str) {
            self.calls
                .lock()
                .unwrap()
                .push((output_dir.to_path_buf(), playlist_stem.to_owned()));
            FsFileCleaner.delete_partial_stream_files(output_dir, playlist_stem);
        }
    }

    #[tokio::test]
    async fn kill_transcoding_jobs_kills_child_and_deletes_partial_files() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned(), "out1.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let cleaner = RecordingCleaner {
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let m = TranscodeManagerImpl::with_file_cleaner(NoopSessionReporter, cleaner.clone());
        let st = state(&playlist, Some(&playlist));
        m.start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();
        assert!(playlist.exists());

        m.kill_transcoding_jobs("dev", Some("sess"), true)
            .await
            .unwrap();

        assert_eq!(m.active_job_count(), 0);
        let calls = cleaner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "out"); // playlist stem
        // The partial files (containing the stem "out") are gone.
        assert!(!out_dir.join("out0.ts").exists());
        assert!(!playlist.exists());
    }

    #[tokio::test]
    async fn kill_without_delete_keeps_files() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let handle = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();
        m.kill_and_remove(&handle, false).await;
        assert_eq!(m.active_job_count(), 0);
        assert!(playlist.exists(), "files kept when delete_files=false");
    }

    #[tokio::test]
    async fn idle_reaper_kills_only_expired_consumerless_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let handle = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();

        // Registration counts as recent activity: nothing to reap yet.
        assert!(m.reap_idle_jobs().await.is_empty());
        assert_eq!(m.active_job_count(), 1);

        // Expire the idle window (a fake clock via ping_timeout_ms = -1, which
        // any non-negative elapsed time exceeds)...
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = -1;
        }
        // ...but an active consumer (the guard) still protects the job.
        let guard = m
            .begin_request_guard(&handle.path, handle.job_type)
            .expect("job registered");
        assert!(m.reap_idle_jobs().await.is_empty());
        drop(guard);

        // The guard drop restarted the countdown; re-expire and reap.
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = -1;
        }
        let killed = m.reap_idle_jobs().await;
        assert_eq!(killed.len(), 1);
        assert_eq!(m.active_job_count(), 0, "idle reaper removed the job");
    }

    #[test]
    fn segment_path_mirrors_get_segment_path() {
        use super::{segment_file_extension, segment_path};
        let ext = segment_file_extension(Some("ts"));
        assert_eq!(ext, ".ts");
        let p = segment_path(Path::new("/cache/abcd.m3u8"), 3, &ext);
        assert_eq!(p, Path::new("/cache/abcd3.ts"));
        assert_eq!(segment_file_extension(Some("mp4")), ".mp4");
        assert_eq!(segment_file_extension(Some("  ")), ".ts");
        assert_eq!(segment_file_extension(None), ".ts");
    }

    #[test]
    fn stderr_log_tail_reports_last_lines() {
        use super::stderr_log_tail;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("ffmpeg.log");

        // Absent or empty log → no tail appended.
        assert_eq!(stderr_log_tail(&log), "");
        std::fs::write(&log, "\n  \n").expect("write");
        assert_eq!(stderr_log_tail(&log), "");

        // Only the last (up to 4) non-empty lines survive, in order.
        std::fs::write(&log, "one\ntwo\n\nthree\nfour\nfive: Stale file handle\n").expect("write");
        assert_eq!(
            stderr_log_tail(&log),
            "; stderr tail: two | three | four | five: Stale file handle"
        );
    }
}
