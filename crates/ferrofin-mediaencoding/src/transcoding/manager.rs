//! Port of the job-registry half of `TranscodeManager`, plus the `StartFfMpeg`
//! spawn orchestration.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{
    TranscodeManager, TranscodingJobHandle, TranscodingJobType, TranscodingProgress,
};

use ferrofin_model::entities::HardwareAccelerationType;
use ferrofin_model::session::TranscodingInfo;

use crate::encoding_helper::EncodingJobInfo;

use super::ffmpeg_log::shown_arguments;
use super::fs_wait::FsWaiter;
use super::limits::{StartFfmpegError, TranscodeLimits, TranscodeRefused};
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

/// The **fallback** tick of the "wait until the segment file exists" loops, in
/// milliseconds. The loops wake on inotify ([`FsWaiter`]) the instant ffmpeg
/// writes the file; this tick only bounds how late a process exit that produced
/// no filesystem event is noticed. Same value as upstream's `Task.Delay(100)`
/// in `StartFfMpeg` / `GetSegmentResult`, which poll unconditionally.
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
    /// The server's **log directory** (`ApplicationPaths.LogDirectoryPath`).
    ///
    /// [`start_ffmpeg`](TranscodeManagerImpl::start_ffmpeg) names the file
    /// inside it exactly as `TranscodeManager.StartFfMpeg` does — see
    /// [`ffmpeg_log_file_name`]. Putting the ffmpeg stderr log *there* rather
    /// than beside the playlist is what makes it reachable from
    /// `GET /System/Logs` and downloadable from the dashboard, which is the
    /// whole point of the C# comment on that line ("let's put it in the log
    /// directory").
    pub log_dir: PathBuf,
    /// The process working directory, if any (`workingDirectory`).
    pub working_dir: Option<PathBuf>,
    /// Environment variables for the ffmpeg child, from the hardware input
    /// arguments. Empty for every path that configures itself by argument.
    pub env: Vec<(String, String)>,
    /// The accelerator this job runs on, for the dashboard's transcode panel.
    pub hardware_acceleration_type: HardwareAccelerationType,
    /// The authenticated user the job is for, from the authorization and
    /// never the query; `None` for a userless API key. The per-user cap counts
    /// by it, and only this user's idle jobs from the same device (the state's
    /// `DeviceId`) are evicted for this job.
    pub user_id: Option<uuid::Uuid>,
}

/// Records the current job's startup metadata and returns its display label.
fn trace_transcode_start(program: &str, state: &EncodingJobInfo, arguments: &[String]) -> String {
    // Identify the job on its span (inherited by every event below) and log
    // the start; the full ffmpeg arg vector is diagnostic but verbose → debug.
    let job_span = tracing::Span::current();
    job_span.record(
        "play_session_id",
        state.play_session_id.as_deref().unwrap_or(""),
    );
    job_span.record("device_id", state.device_id.as_deref().unwrap_or(""));
    // What the job is playing, in operator terms ("Library / Series -
    // Episode") — every event under this span names the media, not just ids.
    let display = state.display.label();
    if !display.is_empty() {
        job_span.record("playing", display.as_str());
    }
    tracing::info!(
        transcode_type = ?state.transcoding_type,
        program = %program,
        args = arguments.len(),
        "transcode job starting"
    );
    tracing::debug!(ffmpeg_args = ?shown_arguments(arguments), "ffmpeg arguments");
    display
}

/// What a starting job is doing, for the session layer. Port of the
/// `TranscodingInfo` literal in `TranscodeManager.ReportTranscodingProgress`.
///
/// Startup publishes the known fields immediately. The stderr statistics pump
/// fills framerate, completion and bitrate as ffmpeg reports them.
fn transcoding_info(
    state: &EncodingJobInfo,
    hardware_acceleration_type: HardwareAccelerationType,
) -> TranscodingInfo {
    let bitrate = match (state.output_video_bitrate, state.output_audio_bitrate) {
        (None, None) => None,
        (v, a) => Some(v.unwrap_or(0).saturating_add(a.unwrap_or(0))),
    };
    TranscodingInfo {
        audio_codec: state.actual_output_audio_codec().map(str::to_owned),
        video_codec: state.actual_output_video_codec().map(str::to_owned),
        container: state.output_container.clone(),
        // "Direct" is about the STREAM, not the container: a copied stream is
        // being remuxed, not transcoded, even though a job exists for it.
        is_video_direct: EncodingJobInfo::is_copy_codec(state.output_video_codec.as_deref()),
        is_audio_direct: EncodingJobInfo::is_copy_codec(state.output_audio_codec.as_deref()),
        bitrate,
        framerate: None,
        completion_percentage: None,
        width: state.output_width(),
        height: state.output_height(),
        audio_channels: state.output_audio_channels,
        hardware_acceleration_type: Some(hardware_acceleration_type),
        transcode_reasons: ferrofin_model::session::transcode_reason_list(state.transcode_reasons),
    }
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

    /// Publishes what a starting job is doing, so the dashboard can show it.
    ///
    /// Port of the `ReportTranscodingInfo` call inside
    /// `TranscodeManager.ReportTranscodingProgress`. Upstream only reaches that
    /// from its ffmpeg progress reader, so a job reporting no progress never
    /// appears at all. The runtime publishes startup details here and updates
    /// them when stderr statistics provide framerate and completion values.
    async fn report_started(&self, job: &TranscodingJobHandle, info: TranscodingInfo);

    /// Tells the session layer that `job`'s ffmpeg never started, so its
    /// device stops showing the transcode [`Self::report_started`] published.
    ///
    /// Port of the `ClearTranscodingInfo(state.Request.DeviceId)` in
    /// `TranscodeManager.OnTranscodeFailedToStart`.
    async fn on_job_failed_to_start(&self, job: &TranscodingJobHandle);

    /// Tears a killed `job` down, deleting its partial output when
    /// `delete_files` is set.
    ///
    /// Port of the `KillTranscodingJob` tail (`Stop` +
    /// `DeletePartialStreamFiles` + a conditional
    /// `CloseLiveStreamIfNeededAsync(job.LiveStreamId, job.PlaySessionId)`).
    ///
    /// `close_live_stream` is upstream's own parameter and is INDEPENDENT of
    /// `delete_files`: only the idle reaper passes it (the abandoned client that
    /// will never report a stop). A seek restart and an explicit stop both pass
    /// `false` — the first is about to re-open ffmpeg on the same tuner, and the
    /// second releases through the session layer instead.
    async fn on_job_killed(
        &self,
        job: &TranscodingJobHandle,
        delete_files: bool,
        close_live_stream: bool,
    );
}

/// Forwards through a shared handle, so a composition root can pick its
/// reporter at runtime (`Arc<dyn SessionReporter>`) and still satisfy the
/// manager's `S: SessionReporter` bound.
#[async_trait]
impl<S: SessionReporter + ?Sized> SessionReporter for std::sync::Arc<S> {
    async fn report_progress(&self, job: &TranscodingJobHandle, progress: TranscodingProgress) {
        (**self).report_progress(job, progress).await;
    }

    async fn report_started(&self, job: &TranscodingJobHandle, info: TranscodingInfo) {
        (**self).report_started(job, info).await;
    }

    async fn on_job_failed_to_start(&self, job: &TranscodingJobHandle) {
        (**self).on_job_failed_to_start(job).await;
    }

    async fn on_job_killed(
        &self,
        job: &TranscodingJobHandle,
        delete_files: bool,
        close_live_stream: bool,
    ) {
        (**self)
            .on_job_killed(job, delete_files, close_live_stream)
            .await;
    }
}

/// A [`SessionReporter`] that does nothing (for tests / progressive-only hosts).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSessionReporter;

#[async_trait]
impl SessionReporter for NoopSessionReporter {
    async fn report_progress(&self, _job: &TranscodingJobHandle, _progress: TranscodingProgress) {}
    async fn report_started(
        &self,
        _job: &TranscodingJobHandle,
        _info: ferrofin_model::session::TranscodingInfo,
    ) {
    }
    async fn on_job_failed_to_start(&self, _job: &TranscodingJobHandle) {}
    async fn on_job_killed(
        &self,
        _job: &TranscodingJobHandle,
        _delete_files: bool,
        _close_live_stream: bool,
    ) {
    }
}

/// Deletes a killed HLS job's partial segment files from its cache directory.
///
/// Port of `DeleteHlsPartialStreamFiles`: every file in `output_dir` whose name
/// contains the playlist stem is removed. Behind a seam so kill/cleanup stays
/// unit-testable against a temp dir without touching a real transcode cache.
/// The teardown runs it on the blocking pool (hence `'static`) and waits for it.
pub trait FileCleaner: Send + Sync + 'static {
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
    child: std::sync::Arc<dyn TranscodeChild>,
    /// Encoded statistics and actual HTTP download progress.
    progress: std::sync::Arc<Mutex<super::throttler::ThrottleProgress>>,
    /// Serializes timer commands with teardown.
    controller: std::sync::Arc<tokio::sync::Mutex<ThrottleController>>,
    /// Known source runtime and the current job's seek offset.
    run_time_ticks: Option<i64>,
    start_time_ticks: Option<i64>,
    /// The rolling deletion timer, canceled when this job is removed.
    segment_cleaner: Option<super::segment_cleaner::SegmentCleanerTask>,
    /// Dashboard details updated by stderr statistics.
    info: TranscodingInfo,
    /// Per-segment end-position tracking for the segment cleaner (replaces the
    /// old division-by-segment-length estimate that deleted ahead on MKV remux).
    served_segments: std::sync::Arc<Mutex<std::collections::HashMap<i64, i64>>>,
    /// The segment cache directory to purge on kill (`DeleteHlsPartialStreamFiles`).
    output_dir: PathBuf,
    /// The `.m3u8`/output path whose stem selects the files to delete.
    playlist_path: PathBuf,
    /// The segment file extension (e.g. `.ts`), which names the segment file
    /// `wait_for_segment` waits for.
    segment_extension: String,
    /// The first segment this job's ffmpeg writes: its `-start_number`, or the
    /// hls muxer's default 0. Segments below it, or past a gap in its run, are
    /// an earlier job's (gl-52).
    start_number: i32,
    /// Where this job's ffmpeg stderr is being written — inside the server's
    /// log directory, so a diagnostic can name a file the operator can actually
    /// fetch from `GET /System/Logs/Log`.
    log_path: PathBuf,
}

impl RunningJob {
    fn new(
        child: Box<dyn TranscodeChild>,
        state: &EncodingJobInfo,
        output_dir: PathBuf,
        playlist_path: PathBuf,
        segment_extension: String,
        log_path: PathBuf,
        hardware_acceleration_type: HardwareAccelerationType,
    ) -> Self {
        Self {
            child: std::sync::Arc::from(child),
            progress: std::sync::Arc::new(
                Mutex::new(super::throttler::ThrottleProgress::default()),
            ),
            controller: std::sync::Arc::new(tokio::sync::Mutex::new(ThrottleController {
                keys: None,
                timed: false,
                next_tick: Instant::now() + super::throttler::THROTTLE_INTERVAL,
                paused: false,
                stopped: false,
                last_progress: None,
                size_failure: None,
                panicked: false,
            })),
            run_time_ticks: state.run_time_ticks,
            start_time_ticks: state.base_request.start_time_ticks,
            segment_cleaner: None,
            info: transcoding_info(state, hardware_acceleration_type),
            served_segments: std::sync::Arc::new(Mutex::new(std::collections::HashMap::new())),
            output_dir,
            playlist_path,
            segment_extension,
            // `start_ffmpeg` sets it from the job's arguments.
            start_number: 0,
            log_path,
        }
    }
}

/// The first segment an HLS ffmpeg writes: the value of its `-start_number`,
/// or the hls muxer's default 0.
fn hls_start_number(arguments: &[String]) -> i32 {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "-start_number")
        .and_then(|pair| pair[1].parse().ok())
        .unwrap_or(0)
}

/// Per-job control state, serialized with teardown.
#[allow(clippy::struct_excessive_bools)] // independent flags, not one state machine
struct ThrottleController {
    keys: Option<super::throttler::PauseKeys>,
    /// Whether this job was eligible and live when its timer was armed. Only
    /// such a job may adopt keys the startup probe publishes later.
    timed: bool,
    next_tick: Instant,
    paused: bool,
    stopped: bool,
    last_progress: Option<super::progress::FfmpegProgress>,
    /// The progressive output-size failure last logged at `warn`; cleared by a
    /// successful read.
    size_failure: Option<String>,
    /// Whether this job's controls step has panicked before, so a repeat
    /// logs at `debug`.
    panicked: bool,
}

impl ThrottleController {
    /// The progressive output's size on disk, or `None` when it cannot be read,
    /// which stops throttling the job (a paused job resumes).
    ///
    /// Every due timer reads it, so a failure is logged at `warn` the first time
    /// it is seen and at `debug` while it repeats (LOGGING.md §1), as
    /// [`EncodingOptionsReader`](crate::configuration::reader::EncodingOptionsReader)
    /// does for the settings read. A successful read re-arms the warning.
    fn output_length(&mut self, path: &str) -> Option<i64> {
        match std::fs::metadata(path) {
            Ok(metadata) => {
                self.size_failure = None;
                Some(i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            }
            Err(error) => {
                let error = error.to_string();
                if self.size_failure.as_ref() == Some(&error) {
                    tracing::debug!(path, %error, "cannot read the progressive transcode's output size");
                } else {
                    tracing::warn!(path, %error, "cannot read the progressive transcode's output size; not throttling it until it can be read");
                    self.size_failure = Some(error);
                }
                None
            }
        }
    }

    /// The job's pause keys, adopting ones the startup probe published after
    /// its timer was armed. A job that never got a timer (ineligible) never
    /// adopts keys.
    fn keys_now(
        &mut self,
        slot: &super::throttler::PauseKeySlot,
    ) -> Option<super::throttler::PauseKeys> {
        if self.keys.is_none() && self.timed {
            self.keys = slot.get();
        }
        self.keys
    }
}

/// The response-body lifetime keeps the producing job alive and records reads.
struct DownloadObserver<S: SessionReporter, C: FileCleaner> {
    manager: std::sync::Weak<TranscodeManagerImpl<S, C>>,
    handle: TranscodingJobHandle,
    child: std::sync::Arc<dyn TranscodeChild>,
    progress: std::sync::Arc<Mutex<super::throttler::ThrottleProgress>>,
    ending_ticks: Option<i64>,
    segment_index: Option<i64>,
    served_segments: std::sync::Arc<Mutex<std::collections::HashMap<i64, i64>>>,
    completed: std::sync::atomic::AtomicBool,
}

impl<S: SessionReporter, C: FileCleaner> ferrofin_traits::media_encoding::TranscodeStreamProgress
    for DownloadObserver<S, C>
{
    fn add_bytes(&self, bytes: usize) {
        if self.handle.job_type == TranscodingJobType::Progressive {
            let mut progress = self.progress.lock().expect("progress lock poisoned");
            progress.bytes_downloaded = progress
                .bytes_downloaded
                .saturating_add(i64::try_from(bytes).unwrap_or(i64::MAX));
        }
    }

    fn complete(&self) {
        if !self
            .completed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
            && let Some(ending_ticks) = self.ending_ticks
        {
            let mut progress = self.progress.lock().expect("progress lock poisoned");
            progress.download_position_ticks = Some(
                progress
                    .download_position_ticks
                    .unwrap_or(ending_ticks)
                    .max(ending_ticks),
            );
            if let Some(idx) = self.segment_index
                && idx >= 0
            {
                self.served_segments
                    .lock()
                    .expect("served_segments lock poisoned")
                    .insert(idx, ending_ticks);
            }
        }
    }

    fn has_exited(&self) -> bool {
        self.child.has_exited()
    }

    fn is_progressive(&self) -> bool {
        self.handle.job_type == TranscodingJobType::Progressive
    }
}

impl<S: SessionReporter, C: FileCleaner> Drop for DownloadObserver<S, C> {
    fn drop(&mut self) {
        ferrofin_traits::media_encoding::TranscodeStreamProgress::complete(self);
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let mut jobs = manager.jobs.lock().expect("jobs lock poisoned");
        if let Some(job) = jobs.iter_mut().find(|job| {
            job.running
                .as_ref()
                .is_some_and(|running| std::sync::Arc::ptr_eq(&running.progress, &self.progress))
        }) {
            job.active_request_count = (job.active_request_count - 1).max(0);
            job.last_activity = Instant::now();
        }
    }
}

/// A registered transcode job: its identifying [`TranscodingJobHandle`] plus the
/// mutable bookkeeping the registry tracks.
struct RegisteredJob {
    handle: TranscodingJobHandle,
    /// The playing media in operator terms (`TranscodeDisplayNames::label`),
    /// so kill/reap logs name what died; empty when never resolved.
    display: String,
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
    /// The user the job transcodes for, counted against
    /// [`TranscodeLimits::per_user`]; `None` for a userless API key and for
    /// registry-only jobs.
    user_id: Option<uuid::Uuid>,
    /// Whether the job re-encodes any stream, so a [`TranscodeLimits`] cap
    /// counts it ([`EncodingJobInfo::encodes_any_stream`]).
    encodes: bool,
}

impl RegisteredJob {
    fn ping_timeout_for(job_type: TranscodingJobType) -> i64 {
        match job_type {
            TranscodingJobType::Progressive => PROGRESSIVE_PING_TIMEOUT_MS,
            TranscodingJobType::Hls | TranscodingJobType::Dash => HLS_PING_TIMEOUT_MS,
        }
    }

    /// Whether a [`TranscodeLimits`] cap counts this job: it re-encodes and its
    /// process has not exited. A job still starting counts.
    fn holds_transcode_slot(&self) -> bool {
        self.encodes
            && self
                .running
                .as_ref()
                .is_none_or(|running| !running.child.has_exited())
    }

    /// Whether this job may be killed to admit another of `user`'s from
    /// `device`: it is that user's, from that device (its `DeviceId`, ignoring
    /// ASCII case as the kill-by-device path does; upstream's
    /// `KillTranscodingJobs` uses `OrdinalIgnoreCase`), it holds a slot, and
    /// nobody is reading from it right now. A job still starting has its start
    /// as a consumer, unless the client hung up on that start.
    fn is_evictable_for(&self, user: uuid::Uuid, device: &str) -> bool {
        self.user_id == Some(user)
            && self
                .handle
                .device_id
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(device))
            && self.active_request_count <= 0
            && self.holds_transcode_slot()
    }
}

/// Makes room in `jobs` for one more re-encoding job of `user` from `device`,
/// the request's `DeviceId`: `Ok(None)` when there is room, the job taken out
/// to make it, or the cap that refuses it.
///
/// One job is always enough, since every job counted came through here. It is
/// the longest-idle job of the same user on the same device, so "next episode"
/// works while the last episode's job waits for its ping timeout. Another
/// device's job is never taken: an idle job is not always abandoned, since an
/// HLS player between two segment requests has no active request either, and
/// ending it would have that player's next segment restart it and end this
/// job in turn. That start is refused instead, until a job ends. A start
/// without a user or without a device has no idle job to give up.
fn take_job_to_admit(
    jobs: &mut Vec<RegisteredJob>,
    limits: TranscodeLimits,
    user: Option<uuid::Uuid>,
    device: Option<&str>,
) -> Result<Option<RegisteredJob>, TranscodeRefused> {
    let running = |user: Option<uuid::Uuid>| {
        jobs.iter()
            .filter(|job| job.holds_transcode_slot() && (user.is_none() || job.user_id == user))
            .count()
    };
    let server_full = TranscodeLimits::is_full(limits.server, running(None));
    let user_full = user.is_some() && TranscodeLimits::is_full(limits.per_user, running(user));
    if !server_full && !user_full {
        return Ok(None);
    }
    let device = device.filter(|device| !device.is_empty());
    let longest_idle = user.zip(device).and_then(|(user, device)| {
        jobs.iter()
            .enumerate()
            .filter(|(_, job)| job.is_evictable_for(user, device))
            .min_by_key(|(_, job)| job.last_activity)
            .map(|(index, _)| index)
    });
    match longest_idle {
        Some(index) => Ok(Some(jobs.remove(index))),
        None if user_full => Err(TranscodeRefused::User),
        None => Err(TranscodeRefused::Server),
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
    file_cleaner: std::sync::Arc<C>,
    encoding_config:
        Option<std::sync::Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager>>,
    /// The throttle's reads of `encoding_config`, which report a document
    /// that cannot be read once rather than on every due timer.
    throttle_options: crate::configuration::EncodingOptionsReader,
    pause_keys: super::throttler::PauseKeySlot,
    /// The caps on concurrent re-encoding jobs; off unless
    /// [`Self::with_transcode_limits`] sets them.
    limits: TranscodeLimits,
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
            file_cleaner: std::sync::Arc::new(file_cleaner),
            encoding_config: None,
            throttle_options: crate::configuration::EncodingOptionsReader::new(
                "transcode throttle",
            ),
            pause_keys: super::throttler::PauseKeySlot::default(),
            limits: TranscodeLimits::default(),
        }
    }

    /// Caps the concurrent re-encoding jobs, server-wide and per user. A start
    /// past a cap first evicts an idle job of the same user on the same device
    /// and is otherwise refused before anything is created or spawned.
    ///
    /// Divergence from upstream, which has no cap; see
    /// [`limits`](super::limits).
    #[must_use]
    pub fn with_transcode_limits(mut self, limits: TranscodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Uses the persisted encoding document for each running job's timer tick.
    #[must_use]
    pub fn with_encoding_configuration(
        mut self,
        config: std::sync::Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager>,
    ) -> Self {
        self.encoding_config = Some(config);
        self
    }

    /// Fixes the ffmpeg keyboard controls now (`None`: this ffmpeg has none).
    #[must_use]
    pub fn with_pause_keys(self, keys: Option<super::throttler::PauseKeys>) -> Self {
        self.pause_keys.set(keys);
        self
    }

    /// The slot the startup probe publishes the keyboard controls through, when
    /// they are not known at construction. Jobs read it on every throttle tick.
    #[must_use]
    pub fn pause_key_slot(&self) -> super::throttler::PauseKeySlot {
        self.pause_keys.clone()
    }

    /// Tracks reads and completion of a progressive job's output response.
    /// Dynamic HLS segments bind through [`Self::segment_progress`] instead.
    ///
    /// The observer releases its consumer mark on drop, including disconnects.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn stream_progress(
        self: &std::sync::Arc<Self>,
        file: &str,
    ) -> Option<std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>>
    where
        S: 'static,
        C: 'static,
    {
        let file = Path::new(file);
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        let job = jobs.iter_mut().find(|job| {
            job.handle.job_type == TranscodingJobType::Progressive
                && job
                    .running
                    .as_ref()
                    .is_some_and(|running| running.playlist_path == file)
        })?;
        let running = job.running.as_ref()?;
        let observer = DownloadObserver {
            manager: std::sync::Arc::downgrade(self),
            handle: job.handle.clone(),
            child: std::sync::Arc::clone(&running.child),
            progress: std::sync::Arc::clone(&running.progress),
            ending_ticks: None,
            segment_index: None,
            served_segments: std::sync::Arc::clone(&running.served_segments),
            completed: std::sync::atomic::AtomicBool::new(false),
        };
        job.active_request_count += 1;
        job.last_activity = Instant::now();
        Some(std::sync::Arc::new(observer))
    }

    /// Binds a dynamic segment response to its current producing job atomically.
    /// The result owns the old job's progress even after a seek replaces it.
    ///
    /// `segment_index` is the zero-based index of the segment being served
    /// (or negative for init segments, which are not tracked).
    ///
    /// # Panics
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn segment_progress(
        self: &std::sync::Arc<Self>,
        playlist: &str,
        ending_ticks: i64,
        segment_index: i64,
    ) -> Option<std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>>
    where
        S: 'static,
        C: 'static,
    {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        let job = jobs.iter_mut().find(|job| {
            job.handle.job_type == TranscodingJobType::Hls
                && job.handle.path.eq_ignore_ascii_case(playlist)
        })?;
        let running = job.running.as_ref()?;
        let observer = DownloadObserver {
            manager: std::sync::Arc::downgrade(self),
            handle: job.handle.clone(),
            child: std::sync::Arc::clone(&running.child),
            progress: std::sync::Arc::clone(&running.progress),
            ending_ticks: Some(ending_ticks),
            segment_index: Some(segment_index),
            served_segments: std::sync::Arc::clone(&running.served_segments),
            completed: std::sync::atomic::AtomicBool::new(false),
        };
        job.active_request_count += 1;
        job.last_activity = Instant::now();
        Some(std::sync::Arc::new(observer))
    }

    /// Pumps stderr statistics and runs due five-second throttle timers.
    ///
    /// Configuration read failures are logged and resume a paused encoder; they
    /// cannot leave playback stalled behind an unreadable settings document.
    ///
    /// A job whose step panics is logged with its path and session, at
    /// `error` the first time and at `debug` after that; the jobs after it
    /// still get their steps, and its own is retried at the next pass.
    ///
    /// # Panics
    ///
    /// Panics if the registry mutex has been poisoned.
    pub async fn update_transcoding_jobs(&self) {
        self.update_transcoding_jobs_with_options(None).await;
    }

    async fn update_transcoding_jobs_with_options(
        &self,
        override_options: Option<&ferrofin_model::configuration::EncodingOptions>,
    ) {
        let running: Vec<_> = {
            let jobs = self.jobs.lock().expect("jobs lock poisoned");
            jobs.iter()
                .filter_map(|job| {
                    job.running.as_ref().map(|running| {
                        (
                            job.handle.clone(),
                            std::sync::Arc::clone(&running.child),
                            std::sync::Arc::clone(&running.controller),
                            std::sync::Arc::clone(&running.progress),
                            running.run_time_ticks,
                            running.start_time_ticks,
                            running.info.clone(),
                        )
                    })
                })
                .collect()
        };
        for (handle, child, controller, progress, runtime, start, mut info) in running {
            let mut controller = controller.lock().await;
            // One job's panic ends its own step, not the pass: the jobs after
            // it still get theirs (gl-1412), as upstream's per-job timers do.
            let step = async {
                if controller.stopped {
                    return;
                }
                if child.has_exited() {
                    controller.stopped = true;
                    return;
                }
                if let Some(report) = child.progress()
                    && controller.last_progress != Some(report)
                {
                    controller.last_progress = Some(report);
                    if let Some(encoded) = report.for_job(runtime, start) {
                        progress.lock().expect("progress lock poisoned").encoded = encoded;
                        info.framerate = encoded.framerate;
                        info.completion_percentage = encoded.percent_complete;
                        info.bitrate = encoded.bit_rate.or(info.bitrate);
                        self.reporter.report_progress(&handle, encoded).await;
                        if handle
                            .device_id
                            .as_deref()
                            .is_some_and(|device| !device.trim().is_empty())
                        {
                            self.reporter.report_started(&handle, info).await;
                        }
                    }
                }
                let Some(keys) = controller.keys_now(&self.pause_keys) else {
                    return;
                };
                if Instant::now() < controller.next_tick {
                    return;
                }
                controller.next_tick = Instant::now() + super::throttler::THROTTLE_INTERVAL;
                // Each due timer reads the live document once. The 500ms stderr
                // pump never performs settings reads for jobs with no due timer.
                let options = if let Some(options) = override_options {
                    options.clone()
                } else if let Some(config) = &self.encoding_config {
                    self.throttle_options.read(config.as_ref()).await
                } else {
                    ferrofin_model::configuration::EncodingOptions::default()
                };
                let progress = *progress.lock().expect("progress lock poisoned");
                let output_length = if options.enable_throttling
                    && progress.download_position_ticks.unwrap_or(0) <= 0
                    && progress.encoded.bytes_transcoded.is_none()
                    && progress.bytes_downloaded > 0
                    && progress.encoded.position_ticks.unwrap_or(0) > 0
                {
                    controller.output_length(&handle.path)
                } else {
                    None
                };
                let paused = super::throttler::should_pause(&options, progress, output_length);
                if paused != controller.paused {
                    let key = if paused { keys.pause() } else { keys.resume() };
                    match child.write_stdin(key).await {
                        Ok(()) => {
                            controller.paused = paused;
                            tracing::debug!(path = %handle.path, paused, "transcode throttle state changed");
                        }
                        Err(error) => {
                            tracing::error!(path = %handle.path, %error, paused, "controlling transcode throttle failed");
                        }
                    }
                }
            };
            let outcome =
                futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(step)).await;
            if let Err(payload) = outcome {
                let panic = ferrofin_util::panic::message(payload.as_ref());
                let play_session_id = handle.play_session_id.as_deref().unwrap_or("");
                // Once per job: a step that panics on every pass would
                // otherwise repeat it each tick, beside the panic hook's line.
                if std::mem::replace(&mut controller.panicked, true) {
                    tracing::debug!(task = "transcode_controls", path = %handle.path, play_session_id, panic, "transcode job's controls step panicked again");
                } else {
                    tracing::error!(task = "transcode_controls", path = %handle.path, play_session_id, panic, "transcode job's controls step panicked; the other jobs go on, and this one is retried each tick");
                }
            }
        }
    }

    /// Runs the progress pump and due throttle timers until the manager drops.
    ///
    /// A weak reference prevents this composition task from retaining all jobs
    /// after server shutdown. A tick that panics is logged and ends alone
    /// (`survive_panic`): the next tick runs as usual. One job's panicking
    /// step ends only that step ([`Self::update_transcoding_jobs`]).
    pub async fn run_transcode_controls(manager: std::sync::Weak<Self>) {
        let mut failing = false;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let Some(manager) = manager.upgrade() else {
                return;
            };
            survive_panic(
                "transcode_controls",
                &mut failing,
                manager.update_transcoding_jobs(),
            )
            .await;
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
            display: String::new(),
            is_user_paused: false,
            // Consumers are counted by begin/end request pairing (the guard);
            // `start_ffmpeg` holds one over its startup wait. A job enrolled
            // here directly has no consumer, and its idle countdown starts now.
            active_request_count: 0,
            last_activity: std::time::Instant::now(),
            handle: handle.clone(),
            running: None,
            user_id: None,
            encodes: false,
        };
        self.jobs.lock().expect("jobs lock poisoned").push(job);
        handle
    }

    /// [`register_job`](Self::register_job) for a starting job of `user`, past
    /// the [`TranscodeLimits`] check when it `encodes`, returning the idle job
    /// of the same device (`handle.device_id`) it replaces, if any, for the
    /// caller to tear down.
    ///
    /// The count and the insert happen under one lock, so concurrent starts
    /// cannot both pass the check. The job enters with its start as a consumer,
    /// which the caller's [`TranscodeRequestGuard`] releases: under the same
    /// lock, so a concurrent start from the same user and device cannot take
    /// it for idle. An exited job of the same output is dropped from the
    /// registry, with no teardown: the new job writes that output.
    fn register_job_for_user(
        &self,
        handle: &TranscodingJobHandle,
        user: Option<uuid::Uuid>,
        encodes: bool,
    ) -> Result<Option<RegisteredJob>, TranscodeRefused> {
        let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
        let evicted = if encodes {
            take_job_to_admit(&mut jobs, self.limits, user, handle.device_id.as_deref())?
        } else {
            None
        };
        // An exited job of the same output (a failed start, or one whose output
        // is gone) gives way: every lookup is by output path, so it would
        // answer for this one. Upstream adds the new job beside it.
        jobs.retain(|job| {
            !(job.handle.job_type == handle.job_type
                && job.handle.path.eq_ignore_ascii_case(&handle.path)
                && job
                    .running
                    .as_ref()
                    .is_some_and(|running| running.child.has_exited()))
        });
        jobs.push(RegisteredJob {
            ping_timeout_ms: RegisteredJob::ping_timeout_for(handle.job_type),
            display: String::new(),
            is_user_paused: false,
            active_request_count: 1,
            last_activity: std::time::Instant::now(),
            handle: handle.clone(),
            running: None,
            user_id: user,
            encodes,
        });
        Ok(evicted)
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
    /// Port of `TranscodeManager.StartFfMpeg`, past the [`TranscodeLimits`]
    /// check upstream does not have: register the job *first* (so a concurrent
    /// segment request finds it), create the output directory, spawn through
    /// the [`SegmentTranscoder`] seam, then poll until the target
    /// file exists or the process exits — surfacing a non-zero exit as an error.
    /// Runtime statistics and eligible transcode throttle timers begin after
    /// the startup target has appeared; teardown resumes a paused child.
    ///
    /// # Errors
    ///
    /// Returns [`StartFfmpegError::Refused`] when a [`TranscodeLimits`] cap stops
    /// the job, before anything is created, logged or spawned, and
    /// [`StartFfmpegError::Failed`] if the output directory cannot be created,
    /// the process cannot be spawned, ffmpeg exits non-zero, the job is killed
    /// before its first segment appears, or that segment does not appear within
    /// [`WAIT_FOR_FILE_TIMEOUT_MS`]. A job whose ffmpeg exited non-zero stays
    /// registered, as upstream's does, until it is killed or reaped.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[tracing::instrument(
        name = "transcode_job",
        skip_all,
        fields(
            play_session_id = tracing::field::Empty,
            device_id = tracing::field::Empty,
            playing = tracing::field::Empty
        )
    )]
    pub async fn start_ffmpeg(
        &self,
        transcoder: &dyn SegmentTranscoder,
        request: StartFfMpegRequest<'_>,
    ) -> Result<TranscodingJobHandle, StartFfmpegError> {
        let StartFfMpegRequest {
            program,
            state,
            output_path,
            arguments,
            log_dir,
            working_dir,
            env,
            hardware_acceleration_type,
            user_id,
        } = request;

        let handle = TranscodingJobHandle {
            play_session_id: state.play_session_id.clone(),
            path: output_path.to_string_lossy().into_owned(),
            job_type: state.transcoding_type,
            device_id: state.device_id.clone(),
            live_stream_id: state.base_request.live_stream_id.clone(),
        };

        // 0. Admission, ahead of everything else: a refused job leaves no
        // directory, no log file and no line in the log. Register the job here
        // (OnTranscodeBeginning), so a concurrent segment request finds it. The
        // startup wait below is the job's first consumer, as upstream's
        // `OnTranscodeBeginning` starts it at `ActiveRequestCount = 1`: it can
        // outlast the ping timeout (a cold 4K software encode), and the idle
        // reaper must not kill the job this request is waiting on. A guard, not
        // a count that stays: a start cancelled by a client disconnect drops
        // it, so its job is still reaped.
        let evicted = self.register_job_for_user(&handle, user_id, state.encodes_any_stream())?;
        let _startup = TranscodeRequestGuard {
            manager: self,
            handle: handle.clone(),
        };
        if let Some(job) = evicted {
            self.evict_job(job, &handle).await;
        }

        let (log_path, directory, display) =
            match prepare_transcode_output(program, state, output_path, &arguments, &log_dir) {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.remove_job_by_path(&handle.path, handle.job_type);
                    return Err(error.into());
                }
            };
        // Name what the job plays, so a later kill/reap log doesn't reduce to ids.
        self.set_job_display(&handle, &display);

        // 3. Build the spawn request.
        let stderr_log = log_path.clone();
        let start_number = hls_start_number(&arguments);
        let req = SpawnRequest {
            program: program.to_owned(),
            arguments,
            working_dir,
            output_dir: directory.clone(),
            log_path,
            env,
        };

        let segment_extension = segment_file_extension(state.segment_container.as_deref());

        // Tell the session layer what this job is, so the dashboard's transcode
        // panel is not blank. Only meaningful with a device to attach it to.
        if state.device_id.as_deref().is_some_and(|d| !d.is_empty()) {
            self.reporter
                .report_started(&handle, transcoding_info(state, hardware_acceleration_type))
                .await;
        }

        // 5. Spawn; on failure remove the job (OnTranscodeFailedToStart).
        let child = match transcoder.start_transcode(&req).await {
            Ok(child) => child,
            Err(e) => {
                self.remove_job_by_path(&handle.path, handle.job_type);
                self.reporter.on_job_failed_to_start(&handle).await;
                return Err(e.into());
            }
        };

        // 6. Store the live job so kill/cleanup can reach the child + cache dir.
        self.attach_running(
            &handle,
            RunningJob {
                start_number,
                ..RunningJob::new(
                    child,
                    state,
                    directory,
                    output_path.to_path_buf(),
                    segment_extension,
                    stderr_log.clone(),
                    hardware_acceleration_type,
                )
            },
        );

        // 7. Wait until the target file exists OR the process exits, bounded.
        let target = state
            .wait_for_path
            .clone()
            .unwrap_or_else(|| output_path.to_path_buf());
        self.wait_for_first_output(&handle, &target).await?;

        self.wait_for_progressive_buffer(&handle, state).await;

        // 8. A finished job that failed is an error (mirror the FfmpegException).
        self.fail_if_exited_nonzero(&handle, &stderr_log)?;

        // 9. Only a live eligible process receives a timer. The option itself
        // is read on each tick, so enabling it applies to existing eligible jobs.
        if super::throttler::eligible(state)
            && self.with_running(&handle, |running| running.child.has_exited()) == Some(false)
        {
            let controller = self.with_running(&handle, |running| {
                std::sync::Arc::clone(&running.controller)
            });
            if let Some(controller) = controller {
                let mut controller = controller.lock().await;
                controller.keys = self.pause_keys.get();
                controller.timed = true;
                controller.next_tick = Instant::now() + super::throttler::THROTTLE_INTERVAL;
            }
        }
        self.start_segment_cleaner(state, &handle);
        // 10. Return the handle.
        Ok(handle)
    }

    /// `start_ffmpeg`'s wait for `target` (its first segment, or the
    /// progressive output): returns once it exists or the process has exited,
    /// and fails when the job is killed meanwhile or the wait outlasts
    /// [`WAIT_FOR_FILE_TIMEOUT_MS`], which kills it.
    async fn wait_for_first_output(
        &self,
        handle: &TranscodingJobHandle,
        target: &Path,
    ) -> Result<(), String> {
        let waiter = FsWaiter::new(target);
        let deadline = Instant::now() + Duration::from_millis(WAIT_FOR_FILE_TIMEOUT_MS);
        loop {
            let exited = self.with_running(handle, |r| r.child.has_exited());
            if target.exists() || exited == Some(true) {
                return Ok(());
            }
            // The job was killed while it started (a stop report, a device's
            // jobs killed): nothing will ever write the target.
            let Some(false) = exited else {
                return Err(format!(
                    "transcode job was stopped before {} appeared",
                    target.display()
                ));
            };
            if Instant::now() >= deadline {
                self.kill_and_remove(handle, true, false).await;
                return Err(format!(
                    "timed out waiting for {} after {WAIT_FOR_FILE_TIMEOUT_MS}ms",
                    target.display()
                ));
            }
            waiter.wait().await;
        }
    }

    fn start_segment_cleaner(&self, state: &EncodingJobInfo, handle: &TranscodingJobHandle) {
        // Rolling retention has its own twenty-second timer. Enroll eligible
        // live jobs even when the switch is initially off; each tick rereads it.
        if super::segment_cleaner::eligible(state)
            && let Some(config) = &self.encoding_config
        {
            let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
            if let Some(running) = jobs
                .iter_mut()
                .find(|job| {
                    job.handle.job_type == handle.job_type
                        && job.handle.path.eq_ignore_ascii_case(&handle.path)
                })
                .and_then(|job| job.running.as_mut())
                && !running.child.has_exited()
            {
                let progress = std::sync::Arc::clone(&running.progress);
                running.segment_cleaner = Some(super::segment_cleaner::SegmentCleanerTask::start(
                    std::sync::Arc::clone(&running.child),
                    std::sync::Arc::clone(config),
                    running.playlist_path.clone(),
                    std::sync::Arc::new(move || {
                        progress
                            .lock()
                            .expect("progress lock poisoned")
                            .download_position_ticks
                    }),
                    std::sync::Arc::clone(&running.served_segments),
                ));
            }
        }
    }

    /// Performs the progressive-video buffer waits before controller enrollment.
    async fn wait_for_progressive_buffer(
        &self,
        handle: &TranscodingJobHandle,
        state: &EncodingJobInfo,
    ) {
        if state.is_input_video
            && state.transcoding_type == TranscodingJobType::Progressive
            && self.with_running(handle, |running| running.child.has_exited()) == Some(false)
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if state.media_source.read_at_native_framerate
                && self.with_running(handle, |running| running.child.has_exited()) == Some(false)
            {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
    }

    /// After the first-segment wait, turns an already-exited-nonzero ffmpeg into
    /// an error (and logs the silent death); a clean early exit logs at debug.
    ///
    /// The failed job stays registered with its exited process, as upstream's
    /// `StartFfMpeg` throws without removing it: a retry finds its output
    /// through it and ends at the output's end, and the stop report or the
    /// idle reaper removes it with its output (gl-1373).
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
    /// Returns at once for a segment already on disk. Otherwise wakes on the
    /// directory's inotify events (fallback tick
    /// [`SEGMENT_READY_POLL_INTERVAL_MS`]) until the file exists or the job
    /// exits; returns `true` if the segment ends up on disk.
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
        // A segment already written needs no watch; the loop checks again once
        // the watch is armed, so one written in between is not missed.
        if seg.exists() {
            return true;
        }
        let waiter = FsWaiter::new(&seg);

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
                let log = self
                    .with_running(handle, |r| r.log_path.clone())
                    .unwrap_or_default();
                tracing::warn!(
                    segment = index,
                    exit_code = code.unwrap_or(-1),
                    log = %log.display(),
                    "transcode exited before producing segment{}",
                    stderr_log_tail(&log)
                );
                return false;
            }
            waiter.wait().await;
        }
    }

    /// The first segment the running job behind `handle` writes (its
    /// `-start_number`), or `None` when no process is attached to it.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn segment_start_number(&self, handle: &TranscodingJobHandle) -> Option<i32> {
        self.with_running(handle, |running| running.start_number)
    }

    /// Whether the running job behind `handle` has served a segment at or past
    /// `index`, its own or one an earlier job left. Rolling deletion removes
    /// segments only up to one the job has served, so until then every segment
    /// it wrote from `index` on is still on disk. `false` when no process is
    /// attached to it.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn has_served_segment_from(&self, handle: &TranscodingJobHandle, index: i32) -> bool {
        self.with_running(handle, |running| {
            running
                .served_segments
                .lock()
                .expect("served_segments lock poisoned")
                .keys()
                .any(|&served| served >= i64::from(index))
        })
        .unwrap_or(false)
    }

    /// Kills the process behind `handle` (if any) and removes it, deleting its
    /// partial segment files when `delete_files` is set. Shared by the timeout
    /// path, the kill-timer, and `kill_transcoding_jobs`.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub async fn kill_and_remove(
        &self,
        handle: &TranscodingJobHandle,
        delete_files: bool,
        close_live_stream: bool,
    ) {
        let (display, running) = {
            let mut jobs = self.jobs.lock().expect("jobs lock poisoned");
            let idx = jobs.iter().position(|j| {
                j.handle.job_type == handle.job_type
                    && j.handle.path.eq_ignore_ascii_case(&handle.path)
            });
            match idx.map(|i| jobs.remove(i)) {
                Some(job) => (job.display, job.running),
                None => (String::new(), None),
            }
        };
        self.tear_down(handle, &display, running, delete_files, close_live_stream)
            .await;
    }

    /// Kills an idle job taken out of the registry to admit `next`, another of
    /// its user's from the same device. The same teardown as the idle reaper's,
    /// which this job would otherwise have met at its ping timeout, except for
    /// a `next` on the same live stream: as on a seek restart, that one is
    /// about to re-open ffmpeg on the same tuner, so the tuner stays open.
    async fn evict_job(&self, job: RegisteredJob, next: &TranscodingJobHandle) {
        let RegisteredJob {
            handle,
            display: playing_label,
            running,
            ..
        } = job;
        tracing::info!(
            path = %handle.path,
            play_session_id = handle.play_session_id.as_deref().unwrap_or(""),
            device_id = handle.device_id.as_deref().unwrap_or(""),
            playing = %playing_label,
            "killing idle transcode job to admit the same device's next one"
        );
        let same_live_stream =
            handle.live_stream_id.is_some() && handle.live_stream_id == next.live_stream_id;
        self.tear_down(&handle, &playing_label, running, true, !same_live_stream)
            .await;
    }

    /// Stops a job already out of the registry: its process, its rolling
    /// deletion and, when `delete_files` is set, its partial output.
    ///
    /// A panic while stopping the process is logged and the file and session
    /// steps still run, as upstream's `KillTranscodingJob` goes on past a
    /// failed `Stop`: no later sweep or kill would reach this job again.
    async fn tear_down(
        &self,
        handle: &TranscodingJobHandle,
        playing_label: &str,
        running: Option<RunningJob>,
        delete_files: bool,
        close_live_stream: bool,
    ) {
        if let Some(mut running) = running {
            // Stop rolling deletion before process and partial-file teardown.
            drop(running.segment_cleaner.take());
            tracing::info!(
                play_session_id = handle.play_session_id.as_deref().unwrap_or(""),
                playing = %playing_label,
                delete_files,
                "transcode job killed"
            );
            // Stop the timer under the same lock as control writes, resuming
            // before kill so ffmpeg cannot remain in its paused command loop.
            let stop = async {
                let mut controller = running.controller.lock().await;
                controller.stopped = true;
                if controller.paused
                    && let Some(keys) = controller.keys
                {
                    match running.child.write_stdin(keys.resume()).await {
                        Ok(()) => controller.paused = false,
                        Err(error) => {
                            tracing::error!(path = %handle.path, %error, "resuming transcode before teardown failed");
                        }
                    }
                }
                if let Err(error) = running.child.kill().await {
                    tracing::error!(path = %handle.path, %error, "killing ffmpeg at teardown failed; it may still be running");
                }
            };
            if let Err(payload) =
                futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(stop)).await
            {
                tracing::error!(
                    path = %handle.path,
                    play_session_id = handle.play_session_id.as_deref().unwrap_or(""),
                    panic = ferrofin_util::panic::message(payload.as_ref()),
                    "stopping ffmpeg at teardown panicked; it may still be running"
                );
            }
            if delete_files {
                let stem = running
                    .playlist_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // The directory scan and unlinks block, so they run on the
                // blocking pool; awaited, so callers still go on only after them.
                let cleaner = std::sync::Arc::clone(&self.file_cleaner);
                let output_dir = running.output_dir;
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    cleaner.delete_partial_stream_files(&output_dir, &stem);
                })
                .await
                {
                    tracing::error!(path = %handle.path, %error, "deleting a killed transcode's partial files failed");
                }
            }
        }
        self.reporter
            .on_job_killed(handle, delete_files, close_live_stream)
            .await;
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
        let idle: Vec<(TranscodingJobHandle, String)> = {
            let jobs = self.jobs.lock().expect("jobs lock poisoned");
            jobs.iter()
                .filter(|j| {
                    j.active_request_count <= 0
                        && i64::try_from(j.last_activity.elapsed().as_millis()).unwrap_or(i64::MAX)
                            > j.ping_timeout_ms
                })
                .map(|j| (j.handle.clone(), j.display.clone()))
                .collect()
        };
        for (handle, playing_label) in &idle {
            tracing::info!(
                path = %handle.path,
                play_session_id = handle.play_session_id.as_deref().unwrap_or(""),
                device_id = handle.device_id.as_deref().unwrap_or(""),
                playing = %playing_label,
                "killing idle transcode job (no consumer within the ping timeout)"
            );
            // `OnTranscodeKillTimerStopped`: `KillTranscodingJob(job, true, _)` —
            // the consumer is gone for good, so the tuner goes back.
            self.kill_and_remove(handle, true, true).await;
        }
        idle.into_iter().map(|(handle, _)| handle).collect()
    }

    /// Runs [`Self::reap_idle_jobs`], sweeping every `interval`, until the
    /// manager drops. The composition root spawns this once next to the manager.
    ///
    /// The weak reference lets a server lifetime's manager, and the session
    /// graph its reporter holds, go when an in-process restart drops it
    /// (gl-141), as [`Self::run_transcode_controls`] does.
    ///
    /// A sweep that panics is logged and ends alone (`survive_panic`): the
    /// jobs it had not reached are still registered, so the next sweep reaps
    /// them. The job it was tearing down is already out of the registry and
    /// is not retried; a panic while stopping its process does not reach the
    /// sweep, since `tear_down` contains it and still tells the session layer.
    pub async fn run_idle_reaper(manager: std::sync::Weak<Self>, interval: Duration) {
        let mut failing = false;
        loop {
            tokio::time::sleep(interval).await;
            let Some(manager) = manager.upgrade() else {
                return;
            };
            survive_panic(
                "transcode_idle_reaper",
                &mut failing,
                manager.reap_idle_jobs(),
            )
            .await;
        }
    }

    /// Kills every job at the end of a server lifetime, deleting its partial
    /// output and releasing its tuner, whether or not a consumer is still on it.
    ///
    /// The drain has already disconnected every client, and the next lifetime
    /// clears the transcode cache before it starts a job, so nothing could
    /// read these jobs' output again. Upstream leaves them to their kill
    /// timers; killing them here stops an ffmpeg writing into a directory
    /// the next lifetime purges (gl-141).
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    pub async fn shutdown(&self) {
        let jobs = std::mem::take(&mut *self.jobs.lock().expect("jobs lock poisoned"));
        for job in jobs {
            self.tear_down(&job.handle, &job.display, job.running, true, true)
                .await;
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

    /// Milliseconds since the job registered for `path`/`job_type` last saw
    /// activity — the value [`Self::reap_idle_jobs`] compares against the job's
    /// ping timeout. `None` when no such job is registered.
    ///
    /// Lets a caller (and the tests around the serve paths) verify that a
    /// keep-alive actually landed on the job it was meant for.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn millis_since_activity(&self, path: &str, job_type: TranscodingJobType) -> Option<u128> {
        let jobs = self.jobs.lock().expect("jobs lock poisoned");
        jobs.iter()
            .find(|j| j.handle.job_type == job_type && j.handle.path.eq_ignore_ascii_case(path))
            .map(|j| j.last_activity.elapsed().as_millis())
    }

    /// Marks a consumer active on the job that *owns* `file_path` — the job
    /// whose playlist stem prefixes the file's (`{stem}.m3u8` itself, or one of
    /// its `{stem}{n}{ext}` segments).
    ///
    /// Port of `HlsSegmentController.GetFileResult`, which resolves the owning
    /// playlist for a served transcode-cache file and calls
    /// `OnTranscodeBeginRequest` on it. Upstream finds it by scanning the
    /// transcode directory; the registry already knows every live job's
    /// playlist path, so this is the same answer without the directory read.
    ///
    /// # Panics
    ///
    /// Panics if the internal job-registry mutex has been poisoned.
    #[must_use]
    pub fn begin_request_guard_for_file(
        &self,
        file_path: &Path,
        job_type: TranscodingJobType,
    ) -> Option<TranscodeRequestGuard<'_, S, C>> {
        // A case-insensitive prefix test on the raw bytes: it runs once per
        // live job on every legacy segment request, so the test allocates
        // nothing.
        let stem = file_path.file_stem()?.as_encoded_bytes();
        let owner = {
            let jobs = self.jobs.lock().expect("jobs lock poisoned");
            jobs.iter()
                .filter(|j| j.handle.job_type == job_type)
                .find(|j| {
                    Path::new(&j.handle.path).file_stem().is_some_and(|s| {
                        let s = s.as_encoded_bytes();
                        stem.get(..s.len())
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(s))
                    })
                })
                .map(|j| j.handle.path.clone())
        }?;
        self.begin_request_guard(&owner, job_type)
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

    /// Names what the registered job for `handle` is playing (the label the
    /// kill/reap logs print). A no-op for an empty label or an unknown job.
    fn set_job_display(&self, handle: &TranscodingJobHandle, display: &str) {
        if display.is_empty() {
            return;
        }
        if let Some(job) = self
            .jobs
            .lock()
            .expect("jobs lock poisoned")
            .iter_mut()
            .find(|j| {
                j.handle.job_type == handle.job_type
                    && j.handle.path.eq_ignore_ascii_case(&handle.path)
            })
        {
            display.clone_into(&mut job.display);
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

/// Runs one pass of the singleton loop `task`, so that a panic ends the pass
/// and not the loop, whose next tick is the retry (LOGGING.md §7).
///
/// `failing` carries the loop's streak: the first panic logs at `error`, the
/// ones straight after it at `debug` (a poisoned lock would otherwise repeat
/// it every tick, beside the panic hook's own line), and the first pass to
/// finish after them at `info`.
async fn survive_panic(task: &'static str, failing: &mut bool, pass: impl Future) {
    match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(pass)).await {
        Ok(_) => {
            if std::mem::take(failing) {
                tracing::info!(task, "transcode loop recovered: a pass finished again");
            }
        }
        Err(payload) => {
            let panic = ferrofin_util::panic::message(payload.as_ref());
            if std::mem::replace(failing, true) {
                tracing::debug!(task, panic, "transcode loop pass panicked again");
            } else {
                tracing::error!(
                    task,
                    panic,
                    "transcode loop pass panicked; the loop goes on at its next tick"
                );
            }
        }
    }
}

/// The segment file extension for `segment_container` (`.ts` default).
///
/// Port of `EncodingHelper.GetSegmentFileExtension` (mirrors the `ferrofin-hls`
/// helper) so the cache-dir naming here agrees with the playlist generator.
fn segment_file_extension(segment_container: Option<&str>) -> String {
    // Case-insensitive, like the `ferrofin-hls` helper: a `segmentContainer=MP4`
    // request must not have this poll `.ts` while the playlist and ffmpeg use
    // `.mp4` (the wait would then only ever time out).
    match segment_container.map(str::trim).filter(|s| !s.is_empty()) {
        Some(container) if container.eq_ignore_ascii_case("mp4") => ".mp4".to_owned(),
        _ => ".ts".to_owned(),
    }
}

/// Readies the disk for an admitted job and logs its start: its ffmpeg log path,
/// its output directory (`Directory.CreateDirectory(Path.GetDirectoryName(outputPath))`)
/// and its display label.
fn prepare_transcode_output(
    program: &str,
    state: &EncodingJobInfo,
    output_path: &Path,
    arguments: &[String],
    log_dir: &Path,
) -> Result<(PathBuf, PathBuf, String), String> {
    let log_path = prepare_ffmpeg_log(log_dir, state)?;
    let display = trace_transcode_start(program, state, arguments);
    let directory = output_path
        .parent()
        .ok_or_else(|| format!("output path {} has no parent", output_path.display()))?
        .to_path_buf();
    std::fs::create_dir_all(&directory)
        .map_err(|e| format!("create output dir {}: {e}", directory.display()))?;
    Ok((log_path, directory, display))
}

/// Resolves this job's ffmpeg stderr log path inside `log_dir`, creating the
/// directory first.
///
/// Upstream opens the `FileStream` directly on
/// `ApplicationPaths.LogDirectoryPath`, which its Serilog sink has already
/// created at boot; here the directory may not exist yet on a fresh install, and
/// a missing one would fail the spawn.
fn prepare_ffmpeg_log(log_dir: &Path, state: &EncodingJobInfo) -> Result<PathBuf, String> {
    std::fs::create_dir_all(log_dir)
        .map_err(|e| format!("create log dir {}: {e}", log_dir.display()))?;
    let path = log_dir.join(ffmpeg_log_file_name(state));
    tracing::debug!(log = %path.display(), "ffmpeg stderr log");
    Ok(path)
}

/// The ffmpeg stderr log file name for `state`, inside the server log directory.
///
/// Port of the naming block of `TranscodeManager.StartFfMpeg`
/// (`MediaBrowser.MediaEncoding/Transcoding/TranscodeManager.cs:450-467` at
/// `v10.11.8`):
///
/// ```text
/// var logFilePrefix = "FFmpeg.Transcode-";
/// if (state.VideoRequest is not null && IsCopyCodec(state.OutputVideoCodec))
///     logFilePrefix = IsCopyCodec(state.OutputAudioCodec) ? "FFmpeg.Remux-" : "FFmpeg.DirectStream-";
/// if (state.VideoRequest is null && IsCopyCodec(state.OutputAudioCodec))
///     logFilePrefix = "FFmpeg.Remux-";
/// … $"{logFilePrefix}{DateTime.Now:yyyy-MM-dd_HH-mm-ss}_{state.Request.MediaSourceId}_{Guid.NewGuid().ToString()[..8]}.log"
/// ```
///
/// Two faithful substitutions, both because the value C# reads does not exist
/// under that name here: `state.VideoRequest is not null` is
/// [`EncodingJobInfo::is_input_video`] (the same predicate the hardware-decode
/// port already spells that way), and `Request.MediaSourceId` is read from the
/// resolved `MediaSource.Id`, which is the id the request pinned.
///
/// `DateTime.Now` is **local** time upstream, and stays local here — these names
/// are read by a human next to `log_*.log` files stamped the same way.
#[must_use]
pub fn ffmpeg_log_file_name(state: &EncodingJobInfo) -> String {
    let video_copy = EncodingJobInfo::is_copy_codec(state.output_video_codec.as_deref());
    let audio_copy = EncodingJobInfo::is_copy_codec(state.output_audio_codec.as_deref());
    let prefix = if state.is_input_video && video_copy {
        if audio_copy {
            "FFmpeg.Remux-"
        } else {
            "FFmpeg.DirectStream-"
        }
    } else if !state.is_input_video && audio_copy {
        "FFmpeg.Remux-"
    } else {
        "FFmpeg.Transcode-"
    };
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let media_source_id = state.media_source.id.clone().unwrap_or_default();
    // `Guid.NewGuid().ToString()[..8]` — the first 8 chars of the dashed form,
    // i.e. 8 hex digits, purely to keep two same-second jobs apart.
    let unique: String = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
    format!("{prefix}{stamp}_{media_source_id}_{unique}.log")
}

/// How many trailing bytes of an ffmpeg stderr log [`stderr_log_tail`] reads.
///
/// Comfortably more than the four lines it keeps (ffmpeg's longest lines are a
/// couple of hundred bytes), and small enough that a multi-megabyte log from a
/// long-running job costs one 16 kB read instead of a full slurp.
///
/// ponytail: tuning knob — a candidate server setting if an operator ever wants
/// a longer diagnostic tail.
const STDERR_TAIL_BYTES: u64 = 16 * 1024;

/// Reads at most the last `max_bytes` of `path` as lossy UTF-8, dropping the
/// first (possibly partial) line when the file was longer than that. Empty on
/// any I/O error.
fn read_tail(path: &Path, max_bytes: u64) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let truncated = len > max_bytes;
    if truncated && file.seek(SeekFrom::Start(len - max_bytes)).is_err() {
        return String::new();
    }
    let mut buf = Vec::with_capacity(usize::try_from(len.min(max_bytes)).unwrap_or(0));
    if file.take(max_bytes).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        // The window opened mid-line; that fragment is not a whole log line.
        text.find('\n')
            .map_or(String::new(), |i| text[i + 1..].to_owned())
    } else {
        text
    }
}

/// The last few lines of ffmpeg's stderr log, formatted for appending to the
/// exit-code error (empty when the log is absent or empty).
///
/// A failed transcode's bare exit code says nothing; stderr names the cause
/// (unreadable input, bad arguments).
///
/// Only the last [`STDERR_TAIL_BYTES`] are read. The log is small when a
/// transcode fails to *start*, but this is also called when a long-running job
/// dies mid-stream (`wait_for_segment`), and ffmpeg's stderr
/// grows at roughly 0.4 kB per wall-clock second of encoding (measured): a
/// two-hour job leaves a multi-megabyte log. Reading it whole would put that
/// much blocking I/O — and, via the returned string, that much log line and
/// HTTP error body — on every failed segment request, exactly when a source is
/// killing ffmpeg repeatedly.
fn stderr_log_tail(log_path: &Path) -> String {
    let contents = read_tail(log_path, STDERR_TAIL_BYTES);
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
            // `KillTranscodingJobs` passes `closeLiveStream: false`; the session
            // layer releases the stream on the stop report instead.
            self.kill_and_remove(job, delete_files, false).await;
        }
        Ok(())
    }

    async fn report_transcoding_progress(
        &self,
        job: &TranscodingJobHandle,
        progress: TranscodingProgress,
    ) -> Result<(), ServiceError> {
        if let Some(shared) =
            self.with_running(job, |running| std::sync::Arc::clone(&running.progress))
        {
            shared.lock().expect("progress lock poisoned").encoded = progress;
        }
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
            live_stream_id: None,
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
    #[derive(Default)]
    pub(super) struct RecordingReporter {
        /// `(path, delete_files, close_live_stream)` per teardown.
        pub(super) killed: Mutex<Vec<(String, bool, bool)>>,
        /// `(device_id, accelerator, video_codec)` per started job.
        pub(super) started: Mutex<Vec<(String, super::HardwareAccelerationType, Option<String>)>>,
        /// The device of each job whose ffmpeg never started.
        pub(super) failed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SessionReporter for RecordingReporter {
        async fn report_started(
            &self,
            job: &TranscodingJobHandle,
            info: ferrofin_model::session::TranscodingInfo,
        ) {
            self.started.lock().unwrap().push((
                job.device_id.clone().unwrap_or_default(),
                info.hardware_acceleration_type
                    .unwrap_or(super::HardwareAccelerationType::none),
                info.video_codec,
            ));
        }
        async fn on_job_failed_to_start(&self, job: &TranscodingJobHandle) {
            self.failed
                .lock()
                .unwrap()
                .push(job.device_id.clone().unwrap_or_default());
        }
        async fn report_progress(
            &self,
            _job: &TranscodingJobHandle,
            _progress: TranscodingProgress,
        ) {
        }
        async fn on_job_killed(
            &self,
            job: &TranscodingJobHandle,
            delete_files: bool,
            close_live_stream: bool,
        ) {
            self.killed
                .lock()
                .unwrap()
                .push((job.path.clone(), delete_files, close_live_stream));
        }
    }

    #[tokio::test]
    async fn kill_notifies_reporter_with_delete_flag() {
        let reporter = Arc::new(RecordingReporter::default());
        let m = TranscodeManagerImpl::new(ReporterHandle(Arc::clone(&reporter)));
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        m.kill_transcoding_jobs("dev1", Some("s1"), true)
            .await
            .unwrap();
        let killed = reporter.killed.lock().unwrap();
        assert_eq!(killed.len(), 1);
        // `KillTranscodingJobs` is `closeLiveStream: false`: the stop report
        // releases the live stream, not the kill.
        assert_eq!(killed[0], ("/t/a.m3u8".to_owned(), true, false));
    }

    #[tokio::test]
    async fn only_the_idle_reaper_asks_teardown_to_release_the_live_stream() {
        // Upstream's two callers differ exactly here: `OnTranscodeKillTimerStopped`
        // passes `closeLiveStream: true` (the consumer is gone for good),
        // `KillTranscodingJobs` passes `false`.
        let reporter = Arc::new(RecordingReporter::default());
        let m = TranscodeManagerImpl::new(ReporterHandle(Arc::clone(&reporter)));
        let job = handle("s2", "/t/b.m3u8", TranscodingJobType::Hls, "dev2");
        m.register_job(job);
        // No consumer, and the ping timeout already elapsed → reaped.
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = -1;
        }
        let reaped = m.reap_idle_jobs().await;
        assert_eq!(reaped.len(), 1);
        let killed = reporter.killed.lock().unwrap();
        assert_eq!(killed[0], ("/t/b.m3u8".to_owned(), true, true));
    }

    /// Adapter so an `Arc<RecordingReporter>` satisfies the `SessionReporter`
    /// bound by value.
    pub(super) struct ReporterHandle(pub(super) Arc<RecordingReporter>);

    #[async_trait]
    impl SessionReporter for ReporterHandle {
        async fn report_started(
            &self,
            job: &TranscodingJobHandle,
            info: ferrofin_model::session::TranscodingInfo,
        ) {
            self.0.report_started(job, info).await;
        }
        async fn on_job_failed_to_start(&self, job: &TranscodingJobHandle) {
            self.0.on_job_failed_to_start(job).await;
        }
        async fn report_progress(&self, job: &TranscodingJobHandle, progress: TranscodingProgress) {
            self.0.report_progress(job, progress).await;
        }
        async fn on_job_killed(
            &self,
            job: &TranscodingJobHandle,
            delete_files: bool,
            close_live_stream: bool,
        ) {
            self.0
                .on_job_killed(job, delete_files, close_live_stream)
                .await;
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

    /// A reporter whose first `panics` teardowns panic, as a fault part-way
    /// through a sweep would, and which records the paths of the rest.
    #[derive(Default)]
    struct FaultyReporter {
        panics: std::sync::atomic::AtomicUsize,
        killed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SessionReporter for FaultyReporter {
        async fn report_started(
            &self,
            _job: &TranscodingJobHandle,
            _info: ferrofin_model::session::TranscodingInfo,
        ) {
        }
        async fn on_job_failed_to_start(&self, _job: &TranscodingJobHandle) {}
        async fn report_progress(
            &self,
            _job: &TranscodingJobHandle,
            _progress: TranscodingProgress,
        ) {
        }
        async fn on_job_killed(
            &self,
            job: &TranscodingJobHandle,
            _delete_files: bool,
            _close_live_stream: bool,
        ) {
            use std::sync::atomic::Ordering;
            if self
                .panics
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                panic!("teardown of {} failed", job.path);
            }
            self.killed.lock().unwrap().push(job.path.clone());
        }
    }

    /// A sweep that panics ends that sweep only: the reaper logs it with its
    /// `task` and sweeps again at the next tick, reaping the jobs the panic cut
    /// it off from. A second panic in a row is not logged above `debug`, and
    /// the first sweep to finish after them says so at `info`.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_sweep_is_logged_and_the_reaper_sweeps_again_next_tick() {
        use crate::configuration::reader::tests::CapturedLogs;
        let reporter = Arc::new(FaultyReporter {
            panics: 2.into(),
            ..FaultyReporter::default()
        });
        let m = Arc::new(TranscodeManagerImpl::new(Arc::clone(&reporter)));
        for (session, path) in [
            ("s1", "/t/a.m3u8"),
            ("s2", "/t/b.m3u8"),
            ("s3", "/t/c.m3u8"),
        ] {
            m.register_job(handle(session, path, TranscodingJobType::Hls, "dev1"));
        }
        for job in m.jobs.lock().unwrap().iter_mut() {
            job.ping_timeout_ms = -1;
        }

        let logs = CapturedLogs::default();
        let guard = logs.install();
        let reaper = tokio::spawn(TranscodeManagerImpl::run_idle_reaper(
            Arc::downgrade(&m),
            std::time::Duration::from_secs(10),
        ));
        // Each sweep reaches one job: the first two panic in its teardown.
        for _ in 0..60 {
            if m.active_job_count() == 0 && !reporter.killed.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        drop(guard);

        assert!(!reaper.is_finished(), "the reaper ended at its first panic");
        reaper.abort();
        assert_eq!(*reporter.killed.lock().unwrap(), ["/t/c.m3u8"]);
        assert_eq!(m.active_job_count(), 0);
        let reaper_lines = |level| -> Vec<String> {
            logs.lines_at(level)
                .into_iter()
                .filter(|line| line.contains("task=\"transcode_idle_reaper\""))
                .collect()
        };
        let errors = reaper_lines("ERROR");
        assert!(
            errors.len() == 1 && errors[0].contains("teardown of /t/a.m3u8 failed"),
            "{errors:#?}"
        );
        let repeats = reaper_lines("DEBUG");
        assert!(
            repeats.len() == 1 && repeats[0].contains("teardown of /t/b.m3u8 failed"),
            "{repeats:#?}"
        );
        let recovered = reaper_lines("INFO");
        assert_eq!(recovered.len(), 1, "{recovered:#?}");
    }

    /// gl-141: the reaper holds its manager weakly, so dropping a server
    /// lifetime's manager ends the reaper instead of keeping the manager, and
    /// the session graph its reporter holds, alive across an in-process restart.
    #[tokio::test(start_paused = true)]
    async fn the_idle_reaper_ends_once_its_manager_is_dropped() {
        let m = Arc::new(manager());
        let reaper = tokio::spawn(TranscodeManagerImpl::run_idle_reaper(
            Arc::downgrade(&m),
            std::time::Duration::from_secs(10),
        ));
        drop(m);
        tokio::time::timeout(std::time::Duration::from_secs(60), reaper)
            .await
            .expect("the reaper outlived its manager")
            .expect("the reaper panicked");
    }

    /// gl-141: the end of a server lifetime kills every job, busy or idle,
    /// deleting its partial output and releasing its tuner, so no ffmpeg
    /// outlives the lifetime that started it.
    #[tokio::test]
    async fn shutdown_kills_and_removes_every_job() {
        let reporter = Arc::new(RecordingReporter::default());
        let m = TranscodeManagerImpl::new(ReporterHandle(Arc::clone(&reporter)));
        m.register_job(handle("s1", "/t/a.m3u8", TranscodingJobType::Hls, "dev1"));
        m.register_job(handle(
            "s2",
            "/t/b.mkv",
            TranscodingJobType::Progressive,
            "dev2",
        ));
        // A consumer mid-request keeps a job off the idle reaper's list.
        m.jobs.lock().unwrap()[0].active_request_count = 1;

        m.shutdown().await;

        assert_eq!(m.active_job_count(), 0);
        let mut killed = reporter.killed.lock().unwrap().clone();
        killed.sort();
        assert_eq!(
            killed,
            [
                ("/t/a.m3u8".to_owned(), true, true),
                ("/t/b.mkv".to_owned(), true, true)
            ]
        );
    }
}

/// Tests for the `start_ffmpeg` spawn orchestration, driven by the
/// [`FakeSegmentTranscoder`] seam so no real ffmpeg is involved.
#[cfg(test)]
mod start_ffmpeg_tests {
    use crate::encoding_helper::TranscodeDisplayNames;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;

    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_traits::media_encoding::{TranscodeManager, TranscodingJobType};

    use crate::encoding_helper::{BaseEncodingJobOptions, EncodingJobInfo};
    use crate::transcoding::segment_transcoder::{FakeScript, FakeSegmentTranscoder};

    use super::{
        FileCleaner, FsFileCleaner, HLS_PING_TIMEOUT_MS, NoopSessionReporter, StartFfMpegRequest,
        TranscodeManagerImpl, ffmpeg_log_file_name,
    };

    /// Builds a `StartFfMpegRequest` for `state`/`output_path` with `args`,
    /// logging into the output directory (the real server passes its log dir).
    fn ffmpeg_req<'a>(
        state: &'a EncodingJobInfo,
        output_path: &'a Path,
        args: Vec<String>,
    ) -> StartFfMpegRequest<'a> {
        StartFfMpegRequest {
            env: Vec::new(),
            hardware_acceleration_type: ferrofin_model::entities::HardwareAccelerationType::none,
            program: "ffmpeg",
            state,
            output_path,
            arguments: args,
            log_dir: output_path
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default(),
            working_dir: None,
            user_id: None,
        }
    }

    /// Builds an HLS `EncodingJobInfo` writing its playlist at `output_path`.
    fn state(output_path: &Path, wait_for: Option<&Path>) -> EncodingJobInfo {
        EncodingJobInfo {
            display: TranscodeDisplayNames::default(),
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
            trimmed_subtitle_path: None,
            transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
            run_time_ticks: None,
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            supported_subtitle_codecs: Vec::new(),
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

    /// `ReportTranscodingProgress` publishes `TranscodeReasons =
    /// state.TranscodeReasons`, as the JSON list in ascending bit order.
    #[test]
    fn the_session_info_reports_the_jobs_transcode_reasons() {
        use ferrofin_model::session::{TranscodeReason, TranscodeReasons};
        let mut st = state(Path::new("/transcodes/out.m3u8"), None);
        st.transcode_reasons =
            TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED | TranscodeReasons::CONTAINER_NOT_SUPPORTED;
        let info = super::transcoding_info(
            &st,
            ferrofin_model::entities::HardwareAccelerationType::none,
        );
        assert_eq!(
            info.transcode_reasons,
            [
                TranscodeReason::ContainerNotSupported,
                TranscodeReason::AudioCodecNotSupported
            ]
        );
    }

    #[test]
    fn the_ffmpeg_log_name_is_jellyfins_prefix_stamp_source_unique() {
        // `TranscodeManager.StartFfMpeg` (v10.11.8:450-467). The whole point of
        // the name is that it lands in the LOG directory and is recognisable
        // there: `GET /System/Logs` lists it and the dashboard downloads it.
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("out.m3u8");
        let mut st = state(&playlist, None);
        st.media_source = MediaSourceInfo {
            id: Some("abc123".to_owned()),
            ..MediaSourceInfo::default()
        };

        // Video job, nothing copied -> full transcode.
        st.output_video_codec = Some("libx264".to_owned());
        st.output_audio_codec = Some("aac".to_owned());
        let name = ffmpeg_log_file_name(&st);
        assert!(name.starts_with("FFmpeg.Transcode-"), "{name}");
        assert_eq!(
            std::path::Path::new(&name)
                .extension()
                .and_then(|e| e.to_str()),
            Some("log"),
            "{name}"
        );
        assert!(name.contains("_abc123_"), "{name}");
        // {prefix}{yyyy-MM-dd_HH-mm-ss}_{mediaSourceId}_{8 hex}.log
        let tail = name.trim_end_matches(".log");
        let unique = tail.rsplit('_').next().unwrap();
        assert_eq!(unique.len(), 8, "{name}");
        assert!(unique.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        let stamp = &name["FFmpeg.Transcode-".len()..]
            ["FFmpeg.Transcode-".len() - "FFmpeg.Transcode-".len().."0000-00-00_00-00-00".len()];
        // `yyyy-MM-dd_HH-mm-ss`, local time as `DateTime.Now` is.
        assert_eq!(stamp.len(), 19, "{name}");
        assert_eq!(&stamp[4..5], "-", "{name}");
        assert_eq!(&stamp[10..11], "_", "{name}");

        // Video copied, audio transcoded -> DirectStream.
        st.output_video_codec = Some("copy".to_owned());
        assert!(ffmpeg_log_file_name(&st).starts_with("FFmpeg.DirectStream-"));

        // Both copied -> Remux.
        st.output_audio_codec = Some("copy".to_owned());
        assert!(ffmpeg_log_file_name(&st).starts_with("FFmpeg.Remux-"));

        // Audio-only job with a copied audio stream -> Remux (the second C# if).
        st.is_input_video = false;
        st.output_video_codec = None;
        assert!(ffmpeg_log_file_name(&st).starts_with("FFmpeg.Remux-"));

        // Audio-only job that re-encodes -> Transcode.
        st.output_audio_codec = Some("libmp3lame".to_owned());
        assert!(ffmpeg_log_file_name(&st).starts_with("FFmpeg.Transcode-"));

        // Two jobs in the same second do not collide.
        assert_ne!(ffmpeg_log_file_name(&st), ffmpeg_log_file_name(&st));
    }

    #[tokio::test]
    async fn the_stderr_log_lands_in_the_log_directory_not_beside_the_playlist() {
        // The regression this pins: Ferrofin used to write
        // `{playlist}.log` into the transcode temp dir, so `GET /System/Logs`
        // never listed a single ffmpeg log and the dashboard could not download
        // one — while Jellyfin's listing is full of `FFmpeg.Transcode-*.log`.
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("session").join("out.m3u8");
        let logs = tmp.path().join("logs");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, None);
        let mut req = ffmpeg_req(&st, &playlist, vec!["-i".to_owned()]);
        req.log_dir = logs.clone();
        m.start_ffmpeg(&fake, req).await.expect("start");

        // The log directory was created (the real spawn opens the file there,
        // the fake only records the request)…
        assert!(logs.is_dir(), "log dir created");
        // …the spawn was told to write into it, under Jellyfin's name…
        let spawned = fake.requests.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        let log_path = &spawned[0].log_path;
        assert_eq!(log_path.parent(), Some(logs.as_path()), "{log_path:?}");
        let name = log_path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("FFmpeg."), "{name}");
        assert_eq!(log_path.extension().and_then(|e| e.to_str()), Some("log"));
        // …and nothing shaped like a log sits next to the playlist any more.
        assert!(!playlist.with_extension("log").exists());
    }

    /// The registry knows the first segment each job writes, from its own
    /// `-start_number` (the hls muxer's default 0 without one), so the
    /// segment path can tell its output from an earlier job's (gl-52).
    #[tokio::test]
    async fn a_job_records_the_first_segment_its_ffmpeg_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeSegmentTranscoder::new(FakeScript {
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let args = |start: &[&str]| {
            ["-i", "in.mkv"]
                .iter()
                .chain(start)
                .map(|arg| (*arg).to_owned())
                .collect::<Vec<_>>()
        };
        let seek = tmp.path().join("seek").join("out.m3u8");
        let st = state(&seek, None);
        let handle = m
            .start_ffmpeg(
                &fake,
                ffmpeg_req(&st, &seek, args(&["-start_number", "15"])),
            )
            .await
            .expect("start");
        assert_eq!(m.segment_start_number(&handle), Some(15));

        let from_zero = tmp.path().join("zero").join("out.m3u8");
        let st = state(&from_zero, None);
        let handle = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &from_zero, args(&[])))
            .await
            .expect("start");
        assert_eq!(m.segment_start_number(&handle), Some(0));

        m.kill_and_remove(&handle, false, false).await;
        assert_eq!(m.segment_start_number(&handle), None, "no job, no start");
    }

    #[tokio::test]
    async fn a_starting_job_publishes_itself_to_the_session_layer() {
        // Without this the dashboard's transcode panel stays blank: the session
        // layer can store and serve the info, but nothing produced it until the
        // job start began reporting.
        use ferrofin_model::entities::HardwareAccelerationType;
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("session").join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let reporter = Arc::new(super::tests::RecordingReporter::default());
        let m = TranscodeManagerImpl::new(super::tests::ReporterHandle(Arc::clone(&reporter)));

        let mut st = state(&playlist, Some(&playlist));
        st.output_video_codec = Some("h264".to_owned());
        // `actual_output_video_codec` is None without a stream to describe,
        // exactly as upstream's is.
        st.video_stream = Some(ferrofin_model::entities_media::MediaStream {
            codec: Some("hevc".to_owned()),
            stream_type: ferrofin_model::entities::MediaStreamType::Video,
            ..ferrofin_model::entities_media::MediaStream::default()
        });
        let mut req = ffmpeg_req(&st, &playlist, vec!["-i".to_owned(), "in.mkv".to_owned()]);
        req.hardware_acceleration_type = HardwareAccelerationType::nvenc;
        m.start_ffmpeg(&fake, req).await.expect("start_ffmpeg");

        assert_eq!(
            *reporter.started.lock().unwrap(),
            vec![(
                "dev".to_owned(),
                HardwareAccelerationType::nvenc,
                // The re-encode target, not the source: a copy would report
                // `hevc` here instead.
                Some("h264".to_owned())
            )]
        );
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
        let reporter = Arc::new(super::tests::RecordingReporter::default());
        let m = TranscodeManagerImpl::new(super::tests::ReporterHandle(Arc::clone(&reporter)));
        let st = state(&playlist, None);
        let err = m
            .start_ffmpeg(&FailingSeam, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "boom");
        // Failed start removes the job and clears the device's transcoding
        // info (OnTranscodeFailedToStart), after the start had published it.
        assert_eq!(m.active_job_count(), 0);
        assert_eq!(reporter.started.lock().unwrap().len(), 1);
        assert_eq!(*reporter.failed.lock().unwrap(), vec!["dev".to_owned()]);
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
        assert!(err.to_string().contains("exited with code 1"), "got: {err}");
        // The job stays registered, as upstream's `StartFfMpeg` throws
        // without removing it (gl-1373).
        assert_eq!(m.active_job_count(), 1);
    }

    /// A progressive `state` writing `output`, with no buffer wait.
    fn progressive(output: &Path) -> EncodingJobInfo {
        EncodingJobInfo {
            transcoding_type: TranscodingJobType::Progressive,
            is_input_video: false,
            ..state(output, None)
        }
    }

    /// A progressive start whose ffmpeg exits non-zero after writing some
    /// output keeps its exited job, as upstream's `StartFfMpeg` does: a retry
    /// of the same request serves the output through that job, so its body
    /// ends at the output's end, and the stop report removes the job and
    /// deletes the output (gl-1373).
    #[tokio::test]
    async fn a_progressive_start_that_exits_nonzero_keeps_its_job_for_the_retry_and_the_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("s").join("out.mkv");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            extra_files: vec!["out.mkv".to_owned()],
            exit_code: 1,
            exits_immediately: true,
            ..FakeScript::default()
        });
        let m = Arc::new(manager());
        let st = progressive(&output);
        let err = m
            .start_ffmpeg(&fake, ffmpeg_req(&st, &output, vec![]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exited with code 1"), "got: {err}");

        let observer = m
            .stream_progress(&output.to_string_lossy())
            .expect("the retry is served through the exited job");
        assert!(
            observer.has_exited(),
            "the retry's body ends at the output's end"
        );
        drop(observer);

        m.kill_transcoding_jobs("dev", Some("sess"), true)
            .await
            .unwrap();
        assert_eq!(m.active_job_count(), 0);
        assert!(!output.exists(), "the stop report deletes the output");
    }

    /// With no stop report, the idle reaper takes the failed start's job and
    /// output at its ping timeout, where upstream's leaked request count keeps
    /// them (gl-1373).
    #[tokio::test]
    async fn the_idle_reaper_takes_a_failed_start_and_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("s").join("out.mkv");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            extra_files: vec!["out.mkv".to_owned()],
            exit_code: 1,
            exits_immediately: true,
            ..FakeScript::default()
        });
        let m = manager();
        let st = progressive(&output);
        m.start_ffmpeg(&fake, ffmpeg_req(&st, &output, vec![]))
            .await
            .unwrap_err();
        // The start's consumer mark ended with it: past the ping timeout
        // (a fake clock: any elapsed time exceeds -1), the job is idle.
        m.jobs.lock().unwrap()[0].ping_timeout_ms = -1;
        assert_eq!(m.reap_idle_jobs().await.len(), 1);
        assert_eq!(m.active_job_count(), 0);
        assert!(!output.exists(), "the reaper deletes the output");
    }

    /// A start for the output of a job whose process has exited replaces that
    /// job: every lookup is by output path, so the exited job would answer
    /// for the new one.
    #[tokio::test]
    async fn a_start_replaces_the_exited_job_of_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("s").join("out.mkv");
        let st = progressive(&output);
        let m = Arc::new(manager());
        let failing = FakeSegmentTranscoder::new(FakeScript {
            exit_code: 1,
            exits_immediately: true,
            ..FakeScript::default()
        });
        m.start_ffmpeg(&failing, ffmpeg_req(&st, &output, vec![]))
            .await
            .unwrap_err();

        let live = FakeSegmentTranscoder::new(FakeScript {
            extra_files: vec!["out.mkv".to_owned()],
            ..FakeScript::default()
        });
        m.start_ffmpeg(&live, ffmpeg_req(&st, &output, vec![]))
            .await
            .expect("the retry starts");
        assert_eq!(m.active_job_count(), 1);
        let observer = m
            .stream_progress(&output.to_string_lossy())
            .expect("the retry's job serves the output");
        assert!(!observer.has_exited(), "the retry's own job answers");
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

    #[tokio::test]
    async fn wait_for_segment_arms_no_watch_for_a_segment_already_on_disk() {
        // A client reading behind ffmpeg asks for segments already written:
        // that wait must not pay for a watcher (a thread, inotify, epoll and
        // an eventfd) it never parks on.
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("s").join("out.m3u8");
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
        let armed = crate::transcoding::fs_wait::ARMED.with(std::cell::Cell::get);
        assert!(m.wait_for_segment(&handle, &playlist, 0).await);
        assert_eq!(
            crate::transcoding::fs_wait::ARMED.with(std::cell::Cell::get),
            armed,
            "a watch was armed for a segment already on disk"
        );
    }

    #[tokio::test]
    async fn wait_for_segment_arms_a_watch_and_finds_a_segment_written_later() {
        // The job is running and segment 1 is not written yet: the wait arms
        // its watch and returns once ffmpeg writes the file.
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
        let armed = crate::transcoding::fs_wait::ARMED.with(std::cell::Cell::get);
        let later = out_dir.join("out1.ts");
        // On this current-thread runtime the writer runs only once the wait
        // parks, i.e. after its first check and the watch being armed.
        tokio::spawn(async move { std::fs::write(later, b"seg").unwrap() });
        assert!(m.wait_for_segment(&handle, &playlist, 1).await);
        assert_eq!(
            crate::transcoding::fs_wait::ARMED.with(std::cell::Cell::get),
            armed + 1
        );
    }

    /// A [`FileCleaner`] that records the deletions it was asked to perform (and
    /// the thread each ran on), and still performs them, so kill/cleanup is
    /// observable over a temp dir.
    #[derive(Clone)]
    struct RecordingCleaner {
        calls: Arc<Mutex<Vec<(std::path::PathBuf, String, std::thread::ThreadId)>>>,
    }

    impl FileCleaner for RecordingCleaner {
        fn delete_partial_stream_files(&self, output_dir: &Path, playlist_stem: &str) {
            FsFileCleaner.delete_partial_stream_files(output_dir, playlist_stem);
            // Recorded once the files are gone, so a kill that returned before
            // its deletion finished leaves no call behind.
            self.calls.lock().unwrap().push((
                output_dir.to_path_buf(),
                playlist_stem.to_owned(),
                std::thread::current().id(),
            ));
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
        // The current-thread runtime's only worker is this thread: the scan and
        // unlinks ran on the blocking pool instead.
        assert_ne!(calls[0].2, std::thread::current().id());
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
        m.kill_and_remove(&handle, false, false).await;
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
        let reaped = m.reap_idle_jobs().await;
        assert!(reaped.is_empty(), "{reaped:?}");
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
        let reaped = m.reap_idle_jobs().await;
        assert!(reaped.is_empty(), "{reaped:?}");
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

    /// Drives `start` until its job is registered with a running child, i.e.
    /// it has reached the wait for its first output.
    async fn reach_startup_wait<F: std::future::Future + Unpin>(
        m: &TranscodeManagerImpl<NoopSessionReporter>,
        start: &mut F,
    ) where
        F::Output: std::fmt::Debug,
    {
        for _ in 0..200 {
            tokio::select! {
                result = &mut *start => panic!("start_ffmpeg ended early: {result:?}"),
                () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
            if m.jobs.lock().unwrap().iter().any(|j| j.running.is_some()) {
                return;
            }
        }
        panic!("start_ffmpeg never reached its startup wait");
    }

    /// Ages every registered job past its idle window (`ping_timeout_ms = -1`
    /// is exceeded by any elapsed time).
    fn expire_idle_windows(m: &TranscodeManagerImpl<NoopSessionReporter>) {
        for job in m.jobs.lock().unwrap().iter_mut() {
            job.ping_timeout_ms = -1;
        }
    }

    /// A first segment slower than the ping timeout (a cold 4K software encode,
    /// a slow tuner) must not get its own job reaped while `start_ffmpeg`
    /// waits for it: upstream's `OnTranscodeBeginning` starts the job with
    /// `ActiveRequestCount = 1`. Once the job is handed back with no consumer,
    /// the idle countdown runs again.
    #[tokio::test]
    async fn a_startup_slower_than_the_ping_timeout_is_not_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("s").join("out.m3u8");
        // Writes nothing and keeps running: the first output is slow to appear.
        let fake = FakeSegmentTranscoder::new(FakeScript::default());
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let mut start = Box::pin(m.start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![])));
        reach_startup_wait(&m, &mut start).await;

        expire_idle_windows(&m);
        assert!(
            m.reap_idle_jobs().await.is_empty(),
            "the reaper killed a job during its own startup wait"
        );

        std::fs::write(&playlist, b"#EXTM3U\n").unwrap();
        start.await.expect("the first output appeared");
        expire_idle_windows(&m);
        assert_eq!(m.reap_idle_jobs().await.len(), 1);
    }

    /// A client that disconnects during the startup wait drops `start_ffmpeg`;
    /// its job must still be reaped, or ffmpeg would run with no consumer
    /// forever (the cast-disconnect leak the reaper exists for).
    #[tokio::test]
    async fn a_cancelled_startup_is_still_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("s").join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript::default());
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let mut start = Box::pin(m.start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![])));
        reach_startup_wait(&m, &mut start).await;

        drop(start);
        expire_idle_windows(&m);
        assert_eq!(m.reap_idle_jobs().await.len(), 1);
        assert_eq!(m.active_job_count(), 0);
    }

    /// A job killed while `start_ffmpeg` waits for its first output (a stop
    /// report, a device's jobs killed) ends the wait at once, instead of
    /// polling a job that no longer exists until the 180 s deadline.
    #[tokio::test]
    async fn a_job_killed_during_startup_ends_the_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let playlist = tmp.path().join("s").join("out.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript::default());
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        let mut start = Box::pin(m.start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![])));
        reach_startup_wait(&m, &mut start).await;

        let handle = m.jobs.lock().unwrap()[0].handle.clone();
        m.kill_and_remove(&handle, true, false).await;
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), start)
            .await
            .expect("the startup wait ended with its job")
            .expect_err("a killed job is not a started transcode");
        assert!(error.to_string().contains("stopped"), "{error}");
    }

    #[tokio::test]
    async fn ping_refreshes_the_idle_countdown() {
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
        m.start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();

        // Expire the idle window (fake clock via ping_timeout_ms = -1)...
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = -1;
        }
        // ...then a session ping (PingTimer) restores the HLS window and
        // restamps activity, so the reaper leaves the job alone.
        m.ping_transcoding_job("sess", None).await.expect("ping");
        assert_eq!(
            m.ping_timeout_for_session("sess"),
            Some(HLS_PING_TIMEOUT_MS),
            "ping restored the job-type default timeout"
        );
        let reaped = m.reap_idle_jobs().await;
        assert!(reaped.is_empty(), "{reaped:?}");
        assert_eq!(m.active_job_count(), 1);

        // Re-expire WITHOUT a ping: the countdown runs out and the job dies.
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = -1;
        }
        let killed = m.reap_idle_jobs().await;
        assert_eq!(killed.len(), 1);
        assert_eq!(m.active_job_count(), 0);
    }

    #[test]
    fn segment_path_mirrors_get_segment_path() {
        use super::{segment_file_extension, segment_path};
        let ext = segment_file_extension(Some("ts"));
        assert_eq!(ext, ".ts");
        let p = segment_path(Path::new("/cache/abcd.m3u8"), 3, &ext);
        assert_eq!(p, Path::new("/cache/abcd3.ts"));
        assert_eq!(segment_file_extension(Some("mp4")), ".mp4");
        // Case-insensitive, as the playlist side is.
        assert_eq!(segment_file_extension(Some("MP4")), ".mp4");
        assert_eq!(segment_file_extension(Some("  ")), ".ts");
        assert_eq!(segment_file_extension(None), ".ts");
    }

    /// The debug line follows `arguments`, which the spawn still gets whole.
    #[test]
    fn the_ffmpeg_args_line_names_a_remote_source_by_its_host() {
        use crate::configuration::reader::tests::CapturedLogs;

        let source = "http://USER:SECRET@127.0.0.1:9/live/USER/SECRET/1.m3u8";
        let arguments: Vec<String> = ["-user_agent", "Box/1 SECRET", "-i", source, "-c:v", "copy"]
            .map(str::to_owned)
            .into();
        let logs = CapturedLogs::default();
        let st = state(Path::new("/t/out.m3u8"), None);
        {
            let _guard = logs.install();
            super::trace_transcode_start("ffmpeg", &st, &arguments);
        }
        let lines = logs.lines_at("DEBUG");
        let line = lines
            .iter()
            .find(|line| line.contains("ffmpeg_args"))
            .expect("the debug line");
        assert!(
            line.contains(
                r#"["-user_agent", "<redacted>", "-i", "http://127.0.0.1:9", "-c:v", "copy"]"#
            ),
            "{line}"
        );
        assert!(!line.contains("SECRET"), "{line}");
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

    #[test]
    fn stderr_log_tail_reads_only_the_end_of_a_huge_log() {
        use super::{STDERR_TAIL_BYTES, read_tail, stderr_log_tail};
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("big.log");

        // A long-running job's log: megabytes of ffmpeg progress lines. Only
        // the last window is read, and the tail still names the real cause.
        let mut body = String::new();
        while u64::try_from(body.len()).unwrap() < 20 * STDERR_TAIL_BYTES {
            body.push_str("frame= 1234 fps= 30 q=28.0 size=  4096kB time=00:12:34.56\n");
        }
        body.push_str("boom: Input/output error\n");
        std::fs::write(&log, &body).expect("write");
        assert!(
            u64::try_from(body.len()).unwrap() > 10 * STDERR_TAIL_BYTES,
            "fixture must be far larger than the read window"
        );

        let window = read_tail(&log, STDERR_TAIL_BYTES);
        assert!(
            u64::try_from(window.len()).unwrap() <= STDERR_TAIL_BYTES,
            "read {} bytes, window is {STDERR_TAIL_BYTES}",
            window.len()
        );
        // The partial first line of the window is dropped, so every retained
        // line is a whole one.
        assert!(window.starts_with("frame="), "{:?}", &window[..40]);

        let tail = stderr_log_tail(&log);
        assert!(tail.ends_with("boom: Input/output error"), "{tail}");

        // ffmpeg writes its progress updates carriage-return separated, so a
        // long run can leave a single line that is itself megabytes long.
        // Slurping the file would put that whole line into the error string,
        // the warn! log and the HTTP 500 body; the bounded read cannot.
        let mut crlog = String::new();
        crlog.push_str(&"x".repeat(usize::try_from(4 * STDERR_TAIL_BYTES).unwrap()));
        crlog.push_str("\nfirst\nsecond\nboom: Input/output error\n");
        std::fs::write(&log, &crlog).expect("write");
        let tail = stderr_log_tail(&log);
        assert!(tail.ends_with("boom: Input/output error"), "{tail}");
        assert!(
            tail.len() < 1024,
            "tail must stay small even beside a megabyte-long line: {} bytes",
            tail.len()
        );
    }

    #[tokio::test]
    async fn begin_request_guard_for_file_finds_the_owning_playlist() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("s");
        let playlist = out_dir.join("a1b2c3.m3u8");
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["a1b2c30.ts".to_owned()],
            extra_files: vec!["a1b2c3.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let m = manager();
        let st = state(&playlist, Some(&playlist));
        m.start_ffmpeg(&fake, ffmpeg_req(&st, &playlist, vec![]))
            .await
            .unwrap();

        // Expire the idle window, then serve one of the job's segments through
        // the legacy route's helper: the keep-alive must restamp activity so
        // the reaper leaves the job alone.
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = -1;
        }
        let guard =
            m.begin_request_guard_for_file(&out_dir.join("a1b2c37.ts"), TranscodingJobType::Hls);
        assert!(guard.is_some(), "segment must resolve to its playlist job");
        drop(guard);
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs[0].ping_timeout_ms = 60_000;
        }
        let reaped = m.reap_idle_jobs().await;
        assert!(reaped.is_empty(), "{reaped:?}");
        assert_eq!(m.active_job_count(), 1);

        // The playlist file itself resolves to the same job...
        assert!(
            m.begin_request_guard_for_file(&playlist, TranscodingJobType::Hls)
                .is_some()
        );
        // ...so does a segment named in another case (upstream finds the
        // playlist by its id, `OrdinalIgnoreCase`)...
        assert!(
            m.begin_request_guard_for_file(&out_dir.join("A1B2C37.ts"), TranscodingJobType::Hls)
                .is_some()
        );
        // ...an unrelated stem does not, nor one shorter than the playlist's,
        // and neither does the wrong job type.
        assert!(
            m.begin_request_guard_for_file(&out_dir.join("deadbeef0.ts"), TranscodingJobType::Hls)
                .is_none()
        );
        assert!(
            m.begin_request_guard_for_file(&out_dir.join("a1b2.ts"), TranscodingJobType::Hls)
                .is_none()
        );
        assert!(
            m.begin_request_guard_for_file(&playlist, TranscodingJobType::Progressive)
                .is_none()
        );
    }
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the source enrollment matrix includes every exclusion and child lifecycle state"
    )]
    async fn rolling_retention_enrolls_only_eligible_live_jobs_after_startup() {
        use super::throttle_tests::LiveOptions;
        use ferrofin_model::configuration::EncodingOptions;
        use ferrofin_model::media_info::MediaProtocol;

        for (runtime, protocol, kind, video, exited, configured, expected) in [
            (
                Some(300),
                MediaProtocol::File,
                TranscodingJobType::Hls,
                true,
                false,
                true,
                true,
            ),
            (
                Some(300),
                MediaProtocol::Http,
                TranscodingJobType::Hls,
                true,
                false,
                true,
                true,
            ),
            (
                Some(299),
                MediaProtocol::File,
                TranscodingJobType::Hls,
                true,
                false,
                true,
                false,
            ),
            (
                None,
                MediaProtocol::Http,
                TranscodingJobType::Hls,
                true,
                false,
                true,
                false,
            ),
            (
                Some(300),
                MediaProtocol::Rtsp,
                TranscodingJobType::Hls,
                true,
                false,
                true,
                false,
            ),
            (
                Some(300),
                MediaProtocol::File,
                TranscodingJobType::Hls,
                false,
                false,
                true,
                false,
            ),
            (
                Some(300),
                MediaProtocol::File,
                TranscodingJobType::Dash,
                true,
                false,
                true,
                false,
            ),
            (
                Some(300),
                MediaProtocol::File,
                TranscodingJobType::Progressive,
                true,
                false,
                true,
                false,
            ),
            (
                Some(300),
                MediaProtocol::File,
                TranscodingJobType::Hls,
                true,
                true,
                true,
                false,
            ),
            (
                Some(300),
                MediaProtocol::File,
                TranscodingJobType::Hls,
                true,
                false,
                false,
                false,
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let playlist = tmp.path().join("out.m3u8");
            let config = Arc::new(LiveOptions::new(EncodingOptions::default()));
            let manager = if configured {
                manager().with_encoding_configuration(config.clone())
            } else {
                manager()
            };
            let fake = FakeSegmentTranscoder::new(FakeScript {
                extra_files: vec!["out.m3u8".to_owned()],
                exits_immediately: exited,
                ..FakeScript::default()
            });
            let mut state = state(&playlist, Some(&playlist));
            state.run_time_ticks = runtime.map(|seconds| seconds * 10_000_000);
            state.media_source.protocol = protocol;
            state.transcoding_type = kind;
            state.is_input_video = video;
            let handle = manager
                .start_ffmpeg(&fake, ffmpeg_req(&state, &playlist, Vec::new()))
                .await
                .unwrap();
            assert_eq!(
                manager.with_running(&handle, |running| running.segment_cleaner.is_some()),
                Some(expected)
            );
            assert_eq!(
                config.reads.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "the first callback is twenty seconds after startup"
            );
            manager.kill_and_remove(&handle, false, false).await;
            assert_eq!(manager.active_job_count(), 0);
            assert!(
                playlist.is_file(),
                "rolling cancellation must not imply final file deletion"
            );
        }
    }
}

#[cfg(test)]
pub(super) mod throttle_tests {
    use super::super::progress::FfmpegProgress;
    use super::super::throttler::{PauseKeys, TICKS_PER_SECOND, ThrottleProgress};
    use super::*;
    use crate::{BaseEncodingJobOptions, TranscodeDisplayNames};
    use ferrofin_model::branding::BrandingOptions;
    use ferrofin_model::configuration::{EncodingOptions, ServerConfiguration};
    use ferrofin_model::dto::MediaSourceInfo;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Shared live-config fake used by the runtime and segment-cleaner tests.
    pub(super) struct LiveOptions {
        pub(super) options: Mutex<EncodingOptions>,
        pub(super) failed: AtomicBool,
        pub(super) reads: AtomicUsize,
    }
    impl LiveOptions {
        pub(super) fn new(options: EncodingOptions) -> Self {
            Self {
                options: Mutex::new(options),
                failed: AtomicBool::new(false),
                reads: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl ferrofin_traits::configuration::ServerConfigurationManager for LiveOptions {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            panic!("this test only reads encoding options")
        }
        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Err(ServiceError::backend("unexpected general config read"))
        }
        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::backend("unexpected general config write"))
        }
        async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
            Err(ServiceError::backend("unexpected branding read"))
        }
        async fn update_branding(&self, _branding: &BrandingOptions) -> Result<(), ServiceError> {
            Err(ServiceError::backend("unexpected branding write"))
        }
        async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.failed.load(Ordering::SeqCst) {
                Err(ServiceError::backend("fixture settings read failed"))
            } else {
                Ok(self.options.lock().unwrap().clone())
            }
        }
    }

    #[derive(Default)]
    struct ChildState {
        progress: Mutex<Option<FfmpegProgress>>,
        events: Mutex<Vec<Vec<u8>>>,
        fail_write: AtomicBool,
        fail_kill: AtomicBool,
        /// Whether `kill` panics.
        panic_kill: AtomicBool,
        exited: AtomicBool,
        /// How many more progress reads panic.
        panics: AtomicUsize,
    }
    struct ControlledChild(Arc<ChildState>);
    #[async_trait]
    impl TranscodeChild for ControlledChild {
        fn has_exited(&self) -> bool {
            self.0.exited.load(Ordering::SeqCst)
        }
        fn exit_code(&self) -> Option<i32> {
            self.has_exited().then_some(0)
        }
        async fn wait(&self) -> i32 {
            self.0.exited.store(true, Ordering::SeqCst);
            0
        }
        async fn kill(&self) -> Result<(), String> {
            self.0.events.lock().unwrap().push(b"kill".to_vec());
            assert!(
                !self.0.panic_kill.load(Ordering::SeqCst),
                "killing ffmpeg panicked"
            );
            if self.0.fail_kill.load(Ordering::SeqCst) {
                return Err("fixture kill failure".to_owned());
            }
            self.0.exited.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn progress(&self) -> Option<FfmpegProgress> {
            if self
                .0
                .panics
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                panic!("reading ffmpeg progress failed");
            }
            *self.0.progress.lock().unwrap()
        }
        async fn write_stdin(&self, bytes: &[u8]) -> Result<(), String> {
            self.0.events.lock().unwrap().push(bytes.to_vec());
            if self.0.fail_write.load(Ordering::SeqCst) {
                Err("fixture stdin failure".to_owned())
            } else {
                Ok(())
            }
        }
    }
    struct ControlledTranscoder(Arc<ChildState>);
    #[async_trait]
    impl SegmentTranscoder for ControlledTranscoder {
        async fn start_transcode(
            &self,
            req: &SpawnRequest,
        ) -> Result<Box<dyn TranscodeChild>, String> {
            std::fs::write(req.output_dir.join("out.m3u8"), b"#EXTM3U\n")
                .map_err(|error| error.to_string())?;
            Ok(Box::new(ControlledChild(Arc::clone(&self.0))))
        }
    }
    fn state(path: &Path) -> EncodingJobInfo {
        EncodingJobInfo {
            display: TranscodeDisplayNames::default(),
            base_request: BaseEncodingJobOptions::default(),
            video_stream: None,
            audio_stream: None,
            subtitle_stream: None,
            media_source: MediaSourceInfo::default(),
            output_video_codec: Some("copy".to_owned()),
            output_audio_codec: Some("copy".to_owned()),
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: Some("ts".to_owned()),
            output_video_sync: None,
            output_file_path: path.to_string_lossy().into_owned(),
            input_container: None,
            is_input_video: true,
            subtitle_delivery_method: ferrofin_model::dlna::SubtitleDeliveryMethod::Encode,
            trimmed_subtitle_path: None,
            transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
            run_time_ticks: Some(600 * TICKS_PER_SECOND),
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            supported_subtitle_codecs: Vec::new(),
            segment_length_secs: 6,
            wait_for_path: Some(path.to_path_buf()),
            segment_container: Some("ts".to_owned()),
            play_session_id: Some("throttle-session".to_owned()),
            device_id: Some("throttle-device".to_owned()),
        }
    }
    async fn fixture(
        keys: Option<PauseKeys>,
    ) -> (
        tempfile::TempDir,
        Arc<TranscodeManagerImpl<NoopSessionReporter>>,
        Arc<ChildState>,
        TranscodingJobHandle,
    ) {
        fixture_of(TranscodeManagerImpl::new(NoopSessionReporter).with_pause_keys(keys)).await
    }
    /// A started job on `manager`, whatever its pause-key state.
    async fn fixture_of(
        manager: TranscodeManagerImpl<NoopSessionReporter>,
    ) -> (
        tempfile::TempDir,
        Arc<TranscodeManagerImpl<NoopSessionReporter>>,
        Arc<ChildState>,
        TranscodingJobHandle,
    ) {
        fixture_job(manager, true).await
    }
    /// As [`fixture_of`], for a video (throttle-eligible) or audio-only job.
    async fn fixture_job<S: SessionReporter>(
        manager: TranscodeManagerImpl<S>,
        is_input_video: bool,
    ) -> (
        tempfile::TempDir,
        Arc<TranscodeManagerImpl<S>>,
        Arc<ChildState>,
        TranscodingJobHandle,
    ) {
        let manager = Arc::new(manager);
        let (directory, child, handle) = start_job(&manager, is_input_video).await;
        (directory, manager, child, handle)
    }
    /// Starts one more job on `manager`, in a directory of its own.
    async fn start_job<S: SessionReporter>(
        manager: &TranscodeManagerImpl<S>,
        is_input_video: bool,
    ) -> (tempfile::TempDir, Arc<ChildState>, TranscodingJobHandle) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("out.m3u8");
        let child = Arc::new(ChildState::default());
        let mut state = state(&path);
        state.is_input_video = is_input_video;
        let handle = manager
            .start_ffmpeg(
                &ControlledTranscoder(Arc::clone(&child)),
                StartFfMpegRequest {
                    program: "controlled-ffmpeg",
                    state: &state,
                    output_path: &path,
                    arguments: Vec::new(),
                    log_dir: directory.path().join("logs"),
                    working_dir: None,
                    user_id: None,
                    env: Vec::new(),
                    hardware_acceleration_type: HardwareAccelerationType::none,
                },
            )
            .await
            .unwrap();
        (directory, child, handle)
    }
    async fn due(
        manager: &TranscodeManagerImpl<NoopSessionReporter>,
        handle: &TranscodingJobHandle,
    ) {
        let controller = manager
            .with_running(handle, |running| Arc::clone(&running.controller))
            .unwrap();
        controller.lock().await.next_tick = Instant::now();
    }
    fn encoded(child: &ChildState, seconds: i64) {
        *child.progress.lock().unwrap() = Some(FfmpegProgress {
            elapsed_ticks: Some(seconds * TICKS_PER_SECOND),
            framerate: Some(25.0),
            bytes_transcoded: Some(1000),
            bit_rate: None,
        });
    }
    fn progress(
        manager: &TranscodeManagerImpl<NoopSessionReporter>,
        handle: &TranscodingJobHandle,
    ) -> Arc<Mutex<ThrottleProgress>> {
        manager
            .with_running(handle, |running| Arc::clone(&running.progress))
            .unwrap()
    }
    fn options(delay: i32) -> EncodingOptions {
        EncodingOptions {
            enable_throttling: true,
            throttle_delay_seconds: delay,
            ..EncodingOptions::default()
        }
    }

    /// Drives one due tick of a job that is eligible, fully downloaded and over
    /// its throttle delay, so whether a pause key is written depends only on the
    /// keys the manager knows by then.
    async fn tick_over_the_delay(
        manager: &TranscodeManagerImpl<NoopSessionReporter>,
        child: &ChildState,
        handle: &TranscodingJobHandle,
    ) {
        encoded(child, 200);
        progress(manager, handle)
            .lock()
            .unwrap()
            .download_position_ticks = Some(6 * TICKS_PER_SECOND);
        due(manager, handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
    }

    #[tokio::test]
    async fn keys_probed_after_the_job_started_apply_on_a_later_tick() {
        // The startup probe runs in the background, so a job can start first.
        let manager = TranscodeManagerImpl::new(NoopSessionReporter);
        let slot = manager.pause_key_slot();
        let (_directory, manager, child, handle) = fixture_of(manager).await;
        tick_over_the_delay(&manager, &child, &handle).await;
        assert!(
            child.events.lock().unwrap().is_empty(),
            "no key is known yet, so nothing is written"
        );
        slot.set(Some(PauseKeys::Patched));
        tick_over_the_delay(&manager, &child, &handle).await;
        assert_eq!(*child.events.lock().unwrap(), vec![b"p".to_vec()]);
    }

    #[tokio::test]
    async fn a_probe_that_found_no_pause_key_leaves_the_job_unthrottled() {
        let manager = TranscodeManagerImpl::new(NoopSessionReporter);
        let slot = manager.pause_key_slot();
        let (_directory, manager, child, handle) = fixture_of(manager).await;
        slot.set(None);
        tick_over_the_delay(&manager, &child, &handle).await;
        assert!(child.events.lock().unwrap().is_empty());
        slot.set(Some(PauseKeys::Patched));
        tick_over_the_delay(&manager, &child, &handle).await;
        assert!(
            child.events.lock().unwrap().is_empty(),
            "a published \"no keys\" does not turn into keys later"
        );
    }

    #[tokio::test]
    async fn a_job_that_was_never_eligible_does_not_adopt_probed_keys() {
        // An audio-only job gets no throttle timer, so published keys never reach it.
        let manager = TranscodeManagerImpl::new(NoopSessionReporter);
        let slot = manager.pause_key_slot();
        let (_directory, manager, child, handle) = fixture_job(manager, false).await;
        slot.set(Some(PauseKeys::Patched));
        tick_over_the_delay(&manager, &child, &handle).await;
        assert!(child.events.lock().unwrap().is_empty());
        // The same keys fixed up front behave the same way.
        let (_directory, manager, child, handle) = fixture_job(
            TranscodeManagerImpl::new(NoopSessionReporter)
                .with_pause_keys(Some(PauseKeys::Patched)),
            false,
        )
        .await;
        tick_over_the_delay(&manager, &child, &handle).await;
        assert!(child.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_first_probe_result_stands() {
        let manager = TranscodeManagerImpl::new(NoopSessionReporter).with_pause_keys(None);
        manager.pause_key_slot().set(Some(PauseKeys::Patched));
        let (_directory, manager, child, handle) = fixture_of(manager).await;
        tick_over_the_delay(&manager, &child, &handle).await;
        assert!(
            child.events.lock().unwrap().is_empty(),
            "keys fixed by the composition root are not overwritten by a later probe"
        );
    }

    #[tokio::test]
    async fn first_timer_waits_and_completed_download_controls_live_pause_resume() {
        let (_directory, manager, child, handle) = fixture(Some(PauseKeys::Patched)).await;
        encoded(&child, 200);
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert!(
            child.events.lock().unwrap().is_empty(),
            "the initial five seconds have not elapsed"
        );
        let observer = manager
            .segment_progress(&handle.path, 6 * TICKS_PER_SECOND, 0)
            .unwrap();
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert!(
            child.events.lock().unwrap().is_empty(),
            "resolution/read before response termination must not invent HLS progress"
        );
        observer.complete();
        assert_eq!(
            progress(&manager, &handle)
                .lock()
                .unwrap()
                .download_position_ticks,
            Some(6 * TICKS_PER_SECOND)
        );
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert_eq!(*child.events.lock().unwrap(), vec![b"p".to_vec()]);
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert_eq!(
            child.events.lock().unwrap().len(),
            1,
            "a paused job receives no duplicate pause"
        );
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(195)))
            .await;
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![b"p".to_vec(), b"u".to_vec()]
        );
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&EncodingOptions::default()))
            .await;
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![
                b"p".to_vec(),
                b"u".to_vec(),
                b"p".to_vec(),
                b"u".to_vec(),
                b"p".to_vec()
            ]
        );
        manager.kill_and_remove(&handle, false, false).await;
        assert_eq!(
            &child.events.lock().unwrap()[5..],
            &[b"u".to_vec(), b"kill".to_vec()]
        );
        assert_eq!(manager.active_job_count(), 0);
    }

    /// A kill that fails at teardown is logged: ffmpeg may still be running.
    #[tokio::test]
    async fn a_failed_kill_at_teardown_is_logged() {
        use crate::configuration::reader::tests::CapturedLogs;
        let (_directory, manager, child, handle) = fixture(None).await;
        child.fail_kill.store(true, Ordering::SeqCst);
        let logs = CapturedLogs::default();
        let guard = logs.install();
        manager.kill_and_remove(&handle, false, false).await;
        drop(guard);
        let errors = logs.lines_at("ERROR");
        assert!(
            errors
                .iter()
                .any(|line| line.contains("killing ffmpeg at teardown failed")
                    && line.contains("fixture kill failure")),
            "{errors:#?}"
        );
        assert_eq!(manager.active_job_count(), 0);
    }

    /// gl-1411: a teardown whose ffmpeg kill panics still deletes the job's
    /// partial files and tells the session layer, which releases its live
    /// stream: the job is already out of the registry, so nothing would retry
    /// them. The panic is logged with the job.
    #[tokio::test]
    async fn a_teardown_whose_kill_panics_still_cleans_up_and_tells_the_session() {
        use super::tests::RecordingReporter;
        use crate::configuration::reader::tests::CapturedLogs;
        let reporter = Arc::new(RecordingReporter::default());
        let (_directory, manager, child, handle) =
            fixture_job(TranscodeManagerImpl::new(Arc::clone(&reporter)), true).await;
        child.panic_kill.store(true, Ordering::SeqCst);

        let logs = CapturedLogs::default();
        let guard = logs.install();
        manager.kill_and_remove(&handle, true, true).await;
        drop(guard);

        assert_eq!(
            *reporter.killed.lock().unwrap(),
            [(handle.path.clone(), true, true)]
        );
        assert!(!Path::new(&handle.path).exists(), "partial files kept");
        assert_eq!(manager.active_job_count(), 0);
        let errors = logs.lines_at("ERROR");
        assert!(
            errors.len() == 1
                && errors[0].contains("killing ffmpeg panicked")
                && errors[0].contains(&handle.path)
                && errors[0].contains("throttle-session"),
            "{errors:#?}"
        );
    }

    #[tokio::test]
    async fn stdin_failures_keep_state_retry_and_teardown_still_kills() {
        let (_directory, manager, child, handle) = fixture(Some(PauseKeys::Legacy)).await;
        encoded(&child, 200);
        progress(&manager, &handle)
            .lock()
            .unwrap()
            .download_position_ticks = Some(6 * TICKS_PER_SECOND);
        child.fail_write.store(true, Ordering::SeqCst);
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert!(
            !manager
                .with_running(&handle, |running| Arc::clone(&running.controller))
                .unwrap()
                .lock()
                .await
                .paused
        );
        child.fail_write.store(false, Ordering::SeqCst);
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert!(
            manager
                .with_running(&handle, |running| Arc::clone(&running.controller))
                .unwrap()
                .lock()
                .await
                .paused
        );
        child.fail_write.store(true, Ordering::SeqCst);
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&EncodingOptions::default()))
            .await;
        manager.kill_and_remove(&handle, false, false).await;
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![
                b"c".to_vec(),
                b"c".to_vec(),
                PauseKeys::Legacy.resume().to_vec(),
                PauseKeys::Legacy.resume().to_vec(),
                b"kill".to_vec()
            ]
        );
        assert!(child.exited.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn live_document_is_read_only_at_due_timer_and_failure_resumes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("out.m3u8");
        let config = Arc::new(LiveOptions::new(options(60)));
        let manager = Arc::new(
            TranscodeManagerImpl::new(NoopSessionReporter)
                .with_pause_keys(Some(PauseKeys::Patched))
                .with_encoding_configuration(config.clone()),
        );
        let child = Arc::new(ChildState::default());
        let state = state(&path);
        let handle = manager
            .start_ffmpeg(
                &ControlledTranscoder(Arc::clone(&child)),
                StartFfMpegRequest {
                    program: "controlled",
                    state: &state,
                    output_path: &path,
                    arguments: Vec::new(),
                    log_dir: directory.path().join("logs"),
                    working_dir: None,
                    user_id: None,
                    env: Vec::new(),
                    hardware_acceleration_type: HardwareAccelerationType::none,
                },
            )
            .await
            .unwrap();
        encoded(&child, 200);
        progress(&manager, &handle)
            .lock()
            .unwrap()
            .download_position_ticks = Some(6 * TICKS_PER_SECOND);
        manager.update_transcoding_jobs().await;
        assert_eq!(config.reads.load(Ordering::SeqCst), 0);
        due(&manager, &handle).await;
        manager.update_transcoding_jobs().await;
        assert_eq!(config.reads.load(Ordering::SeqCst), 1);
        *config.options.lock().unwrap() = EncodingOptions::default();
        due(&manager, &handle).await;
        manager.update_transcoding_jobs().await;
        *config.options.lock().unwrap() = options(60);
        due(&manager, &handle).await;
        manager.update_transcoding_jobs().await;
        config.failed.store(true, Ordering::SeqCst);
        due(&manager, &handle).await;
        manager.update_transcoding_jobs().await;
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![b"p".to_vec(), b"u".to_vec(), b"p".to_vec(), b"u".to_vec()]
        );
    }

    /// A settings file the throttle cannot read is reported once at `warn`,
    /// then at `debug` while it stays broken. The throttle reads it on every
    /// due timer of every running job and carries on with the defaults, so an
    /// `error` per tick would flood the log (LOGGING.md §1).
    #[tokio::test]
    async fn an_unreadable_settings_file_is_warned_about_once_not_every_tick() {
        use crate::configuration::reader::tests::CapturedLogs;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("out.m3u8");
        let config = Arc::new(LiveOptions::new(options(60)));
        config.failed.store(true, Ordering::SeqCst);
        let manager = Arc::new(
            TranscodeManagerImpl::new(NoopSessionReporter)
                .with_pause_keys(Some(PauseKeys::Patched))
                .with_encoding_configuration(config.clone()),
        );
        let child = Arc::new(ChildState::default());
        let state = state(&path);
        let handle = manager
            .start_ffmpeg(
                &ControlledTranscoder(Arc::clone(&child)),
                StartFfMpegRequest {
                    program: "controlled",
                    state: &state,
                    output_path: &path,
                    arguments: Vec::new(),
                    log_dir: directory.path().join("logs"),
                    working_dir: None,
                    user_id: None,
                    env: Vec::new(),
                    hardware_acceleration_type: HardwareAccelerationType::none,
                },
            )
            .await
            .unwrap();

        let logs = CapturedLogs::default();
        let guard = logs.install();
        for _ in 0..3 {
            due(&manager, &handle).await;
            manager.update_transcoding_jobs().await;
        }
        drop(guard);

        assert_eq!(config.reads.load(Ordering::SeqCst), 3);
        assert!(
            logs.lines_at("ERROR").is_empty(),
            "{:#?}",
            logs.lines_at("ERROR")
        );
        let warnings = logs.lines_at("WARN");
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains("transcode throttle"), "{warnings:#?}");
        let repeats = logs
            .lines_at("DEBUG")
            .into_iter()
            .filter(|line| line.contains("cannot read the encoding configuration"))
            .count();
        assert_eq!(repeats, 2);
    }

    /// A progressive output the throttle cannot size is reported once at
    /// `warn`, then at `debug` while it stays unreadable: the job carries on
    /// unthrottled, as upstream's does, so an `error` per due timer would flood
    /// the log (LOGGING.md §1). A successful read re-arms the warning.
    #[tokio::test]
    async fn an_unreadable_progressive_output_is_warned_about_once_not_every_tick() {
        use crate::configuration::reader::tests::CapturedLogs;
        async fn tick(
            manager: &TranscodeManagerImpl<NoopSessionReporter>,
            handle: &TranscodingJobHandle,
        ) {
            due(manager, handle).await;
            manager
                .update_transcoding_jobs_with_options(Some(&options(60)))
                .await;
        }
        let (_directory, manager, child, handle) = fixture(Some(PauseKeys::Patched)).await;
        // ffmpeg reports a position but no byte count and the client has
        // downloaded bytes, so the tick sizes the output file (the progressive
        // branch; the fixture's job type does not enter into it).
        *child.progress.lock().unwrap() = Some(FfmpegProgress {
            elapsed_ticks: Some(200 * TICKS_PER_SECOND),
            framerate: Some(25.0),
            bytes_transcoded: None,
            bit_rate: None,
        });
        progress(&manager, &handle).lock().unwrap().bytes_downloaded = 100;
        // The fixture's transcoder wrote the output; it disappears mid-job.
        std::fs::remove_file(&handle.path).unwrap();
        let warned = |logs: &CapturedLogs| {
            logs.lines_at("WARN")
                .into_iter()
                .filter(|line| line.contains("progressive transcode"))
                .count()
        };

        let logs = CapturedLogs::default();
        let guard = logs.install();
        for _ in 0..3 {
            tick(&manager, &handle).await;
        }
        assert!(
            logs.lines_at("ERROR").is_empty(),
            "{:#?}",
            logs.lines_at("ERROR")
        );
        assert_eq!(warned(&logs), 1, "{:#?}", logs.lines_at("WARN"));
        let repeats = logs
            .lines_at("DEBUG")
            .into_iter()
            .filter(|line| line.contains("progressive transcode"))
            .count();
        assert_eq!(repeats, 2);
        assert!(child.events.lock().unwrap().is_empty(), "no size, no pause");

        // 10000 bytes encoded against 100 downloaded is past the 60 s gap at
        // 200 s encoded, so a good read pauses the job; losing the file again
        // resumes it and warns again.
        std::fs::write(&handle.path, [0u8; 10_000]).unwrap();
        tick(&manager, &handle).await;
        std::fs::remove_file(&handle.path).unwrap();
        tick(&manager, &handle).await;
        drop(guard);
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![b"p".to_vec(), b"u".to_vec()]
        );
        assert_eq!(
            warned(&logs),
            2,
            "a good read re-arms the warning: {:#?}",
            logs.lines_at("WARN")
        );
    }

    /// A job's step that panics ends that step only: the controls loop logs
    /// it with its `task` and pumps the job's progress at the next tick.
    #[tokio::test]
    async fn a_panicking_tick_is_logged_and_the_controls_loop_carries_on() {
        use crate::configuration::reader::tests::CapturedLogs;
        let (_directory, manager, child, handle) = fixture(None).await;
        encoded(&child, 200);
        child.panics.store(1, Ordering::SeqCst);
        tokio::time::pause();

        let logs = CapturedLogs::default();
        let guard = logs.install();
        let controls = tokio::spawn(TranscodeManagerImpl::run_transcode_controls(
            Arc::downgrade(&manager),
        ));
        for _ in 0..10 {
            if progress(&manager, &handle)
                .lock()
                .unwrap()
                .encoded
                .framerate
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        drop(guard);

        assert!(
            !controls.is_finished(),
            "the controls loop ended at its first panic"
        );
        controls.abort();
        assert_eq!(
            progress(&manager, &handle)
                .lock()
                .unwrap()
                .encoded
                .framerate,
            Some(25.0)
        );
        let errors: Vec<_> = logs
            .lines_at("ERROR")
            .into_iter()
            .filter(|line| line.contains("task=\"transcode_controls\""))
            .collect();
        assert!(
            errors.len() == 1 && errors[0].contains("reading ffmpeg progress failed"),
            "{errors:#?}"
        );
    }

    /// gl-1412: a job whose step panics on every pass ends only its own step,
    /// so the job after it in the registry still gets its progress pumped. The
    /// panic is logged with the job, at `error` once and then at `debug`.
    #[tokio::test]
    async fn a_job_that_panics_every_pass_does_not_starve_the_jobs_after_it() {
        use crate::configuration::reader::tests::CapturedLogs;
        let (_broken_directory, manager, broken, broken_handle) = fixture(None).await;
        let (_directory, healthy, handle) = start_job(&manager, true).await;
        encoded(&broken, 200);
        encoded(&healthy, 200);
        broken.panics.store(usize::MAX, Ordering::SeqCst);
        tokio::time::pause();

        let logs = CapturedLogs::default();
        let guard = logs.install();
        let controls = tokio::spawn(TranscodeManagerImpl::run_transcode_controls(
            Arc::downgrade(&manager),
        ));
        // Three passes, each reaching the broken job first.
        tokio::time::sleep(Duration::from_millis(1600)).await;
        drop(guard);
        controls.abort();

        assert_eq!(
            progress(&manager, &handle)
                .lock()
                .unwrap()
                .encoded
                .framerate,
            Some(25.0),
            "the job after the broken one starved"
        );
        let broken_lines = |level| -> Vec<String> {
            logs.lines_at(level)
                .into_iter()
                .filter(|line| {
                    line.contains("task=\"transcode_controls\"")
                        && line.contains(&broken_handle.path)
                })
                .collect()
        };
        let errors = broken_lines("ERROR");
        assert!(
            errors.len() == 1
                && errors[0].contains("reading ffmpeg progress failed")
                && errors[0].contains("throttle-session"),
            "{errors:#?}"
        );
        assert_eq!(
            broken_lines("DEBUG").len(),
            2,
            "{:#?}",
            logs.lines_at("DEBUG")
        );
    }

    #[tokio::test]
    async fn client_ping_does_not_substitute_for_download_and_old_body_cannot_update_seek_replacement()
     {
        let (directory, manager, child, handle) = fixture(Some(PauseKeys::Patched)).await;
        encoded(&child, 200);
        manager
            .ping_transcoding_job("throttle-session", Some(true))
            .await
            .unwrap();
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert!(child.events.lock().unwrap().is_empty());
        let old_progress = progress(&manager, &handle);
        let observer = manager
            .segment_progress(&handle.path, 6 * TICKS_PER_SECOND, 0)
            .unwrap();
        assert_eq!(manager.jobs.lock().unwrap()[0].active_request_count, 1);
        manager.kill_and_remove(&handle, false, false).await;
        let state = state(Path::new(&handle.path));
        let replacement_child = Arc::new(ChildState::default());
        let replacement = manager
            .start_ffmpeg(
                &ControlledTranscoder(Arc::clone(&replacement_child)),
                StartFfMpegRequest {
                    program: "controlled",
                    state: &state,
                    output_path: Path::new(&handle.path),
                    arguments: Vec::new(),
                    log_dir: directory.path().join("logs"),
                    working_dir: None,
                    user_id: None,
                    env: Vec::new(),
                    hardware_acceleration_type: HardwareAccelerationType::none,
                },
            )
            .await
            .unwrap();
        let new_observer = manager
            .segment_progress(&replacement.path, 100 * TICKS_PER_SECOND, 0)
            .unwrap();
        observer.complete();
        drop(observer);
        assert_eq!(
            old_progress.lock().unwrap().download_position_ticks,
            Some(6 * TICKS_PER_SECOND)
        );
        assert_eq!(
            progress(&manager, &replacement)
                .lock()
                .unwrap()
                .download_position_ticks,
            None
        );
        assert_eq!(manager.jobs.lock().unwrap()[0].active_request_count, 1);
        drop(new_observer);
        assert_eq!(manager.jobs.lock().unwrap()[0].active_request_count, 0);
        assert_eq!(
            progress(&manager, &replacement)
                .lock()
                .unwrap()
                .download_position_ticks,
            Some(100 * TICKS_PER_SECOND)
        );
    }

    /// What the segment path reads to tell a job's first segments that rolling
    /// deletion removed from ones it has not written yet (gl-1520).
    #[tokio::test]
    async fn a_job_reports_whether_it_served_a_segment_from_an_index() {
        let (_directory, manager, _child, handle) = fixture(None).await;
        assert!(
            !manager.has_served_segment_from(&handle, 0),
            "nothing served"
        );
        drop(
            manager
                .segment_progress(&handle.path, 6 * TICKS_PER_SECOND, 3)
                .unwrap(),
        );
        assert!(manager.has_served_segment_from(&handle, 3));
        assert!(!manager.has_served_segment_from(&handle, 4));
        manager.kill_and_remove(&handle, false, false).await;
        assert!(!manager.has_served_segment_from(&handle, 0), "no job");
    }

    #[tokio::test]
    async fn repeated_and_out_of_order_completions_are_monotonic() {
        let (_directory, manager, _child, handle) = fixture(None).await;
        for (index, ending) in [(9, 62), (0, 3), (4, 30)] {
            let observer = manager
                .segment_progress(&handle.path, ending * TICKS_PER_SECOND, index)
                .unwrap();
            observer.complete();
            observer.complete();
        }
        assert_eq!(
            progress(&manager, &handle)
                .lock()
                .unwrap()
                .download_position_ticks,
            Some(62 * TICKS_PER_SECOND)
        );
        assert!(
            manager.stream_progress(handle.path.as_str()).is_none(),
            "an HLS job's files bind through segment_progress only"
        );
    }

    #[tokio::test]
    async fn missing_keyboard_support_and_exited_process_never_receive_pause() {
        for keys in [None, Some(PauseKeys::Patched)] {
            let (_directory, manager, child, handle) = fixture(keys).await;
            encoded(&child, 200);
            progress(&manager, &handle)
                .lock()
                .unwrap()
                .download_position_ticks = Some(6 * TICKS_PER_SECOND);
            if keys.is_some() {
                child.exited.store(true, Ordering::SeqCst);
            }
            due(&manager, &handle).await;
            manager
                .update_transcoding_jobs_with_options(Some(&options(60)))
                .await;
            assert!(child.events.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn progressive_reads_seek_offset_file_size_fallback_and_stat_failure_control_pause() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("out.m3u8");
        let manager = Arc::new(
            TranscodeManagerImpl::new(NoopSessionReporter)
                .with_pause_keys(Some(PauseKeys::Patched)),
        );
        let child = Arc::new(ChildState::default());
        let mut state = state(&path);
        state.transcoding_type = TranscodingJobType::Progressive;
        state.base_request.start_time_ticks = Some(60 * TICKS_PER_SECOND);
        let handle = manager
            .start_ffmpeg(
                &ControlledTranscoder(Arc::clone(&child)),
                StartFfMpegRequest {
                    program: "controlled",
                    state: &state,
                    output_path: &path,
                    arguments: Vec::new(),
                    log_dir: directory.path().join("logs"),
                    working_dir: None,
                    user_id: None,
                    env: Vec::new(),
                    hardware_acceleration_type: HardwareAccelerationType::none,
                },
            )
            .await
            .unwrap();
        encoded(&child, 200);
        let observer = manager.stream_progress(handle.path.as_str()).unwrap();
        assert!(observer.is_progressive());
        observer.add_bytes(730);
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert_eq!(
            progress(&manager, &handle)
                .lock()
                .unwrap()
                .encoded
                .position_ticks,
            Some(260 * TICKS_PER_SECOND)
        );
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![b"p".to_vec()],
            "actual byte reads and the seek-adjusted position cross the source estimate"
        );
        *child.progress.lock().unwrap() = Some(FfmpegProgress {
            elapsed_ticks: Some(200 * TICKS_PER_SECOND),
            framerate: Some(25.0),
            bytes_transcoded: None,
            bit_rate: None,
        });
        std::fs::write(&path, vec![0; 1000]).unwrap();
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert_eq!(
            child.events.lock().unwrap().len(),
            1,
            "file size replaces missing encoded byte statistics"
        );
        std::fs::remove_file(&path).unwrap();
        due(&manager, &handle).await;
        manager
            .update_transcoding_jobs_with_options(Some(&options(60)))
            .await;
        assert_eq!(
            *child.events.lock().unwrap(),
            vec![b"p".to_vec(), b"u".to_vec()],
            "failed file metadata must not leave a producer paused"
        );
        drop(observer);
        assert_eq!(manager.jobs.lock().unwrap()[0].active_request_count, 0);
    }
}
