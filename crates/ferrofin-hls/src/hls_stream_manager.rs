//! [`HlsStreamManagerImpl`] — the concrete [`HlsStreamManager`] that ties the
//! transcode runtime (`ferrofin-mediaencoding`) to the HLS playlist generator
//! (`ferrofin-hls`).
//!
//! This is the composition point the Wave-8 server injects into `ferrofin-api`'s
//! `AppState`. It lives in `ferrofin-hls` because that crate is the only one that
//! depends on **both** the [`TranscodeManagerImpl`] (`start_ffmpeg` /
//! `wait_for_segment`, from `ferrofin-mediaencoding`) **and** the
//! [`DynamicHlsPlaylistGenerator`] (its own) — so the wiring belongs here, not in
//! either dependency (`RULES_CODE_REUSE`: the join lives above both).
//!
//! ## What is ported vs. deferred
//!
//! The **orchestration** is ported and unit-tested here against a fake
//! [`SegmentTranscoder`] and a fake [`StreamStatePlanner`]:
//!
//! - playlist generation → [`DynamicHlsPlaylistGenerator::create_main_playlist`];
//! - segment start/reuse → [`TranscodeManagerImpl::start_ffmpeg`] +
//!   [`TranscodeManagerImpl::wait_for_segment`], with the segment file resolved
//!   from the cache dir;
//! - the legacy transcode-folder file resolution +
//!   [`HlsSegmentController.ValidateTranscodePath`] path-traversal guard;
//! - stop-encoding → [`TranscodeManager::kill_transcoding_jobs`].
//!
//! The **un-ported** piece — turning the raw [`HlsStreamRequest`] into a concrete
//! transcode plan (media-source resolution, `StreamState`, and the ffmpeg
//! command-line) — is Jellyfin's `StreamingHelpers.GetStreamingState` +
//! `EncodingHelper.GetCommandLineArguments`, the last large slice of the port.
//! It sits behind the [`StreamStatePlanner`] seam so
//! everything above it stays testable; the Wave-8 wiring supplies the real
//! planner over `ferrofin-core`'s media-source manager + the ported
//! [`EncodingHelper`](ferrofin_mediaencoding::EncodingHelper) arg builder.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_mediaencoding::transcoding::manager::StartFfMpegRequest;
use ferrofin_mediaencoding::transcoding::segment_transcoder::SegmentTranscoder;
use ferrofin_mediaencoding::transcoding::{FsFileCleaner, SessionReporter};
use ferrofin_mediaencoding::{EncodingJobInfo, TranscodeManagerImpl};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{
    HlsStreamManager, HlsStreamRequest, ServedFile, TranscodingJobType,
};
use ferrofin_traits::system::ServerApplicationPaths;

use crate::create_main_playlist_request::CreateMainPlaylistRequest;
use crate::dynamic_hls_playlist_generator::{
    DynamicHlsPlaylistGenerator, EncodingOptionsProvider, TICKS_PER_MILLISECOND,
};

/// A concrete transcode plan for one request: everything the runtime needs that
/// [`HlsStreamRequest`] alone does not carry.
///
/// Port of the useful output of `StreamingHelpers.GetStreamingState` +
/// `EncodingHelper.GetCommandLineArguments` — the fields the ported HLS
/// orchestration consumes. Built by a [`StreamStatePlanner`]; the [`state`] is
/// handed straight to [`TranscodeManagerImpl::start_ffmpeg`].
pub struct TranscodePlan {
    /// The fully-populated job state (media path, run-time ticks, segment
    /// container, output path, session/device ids) for `start_ffmpeg`.
    pub state: EncodingJobInfo,
    /// The `.m3u8` output/playlist path ffmpeg writes (`OutputFilePath` with the
    /// extension changed to `.m3u8`).
    pub playlist_path: PathBuf,
    /// The fully-built ffmpeg command-line arguments.
    pub arguments: Vec<String>,
    /// The absolute source media path (for the playlist generator's
    /// `CreateMainPlaylistRequest.file_path`).
    pub media_path: String,
    /// The total runtime in ticks (for the playlist generator).
    pub run_time_ticks: i64,
    /// The desired segment length in milliseconds (for the playlist generator).
    pub segment_length_ms: i32,
    /// Whether the output video codec is a stream-copy (`IsCopyCodec`) — controls
    /// the playlist generator's `is_remuxing_video` flag.
    pub is_remuxing_video: bool,
    /// The resolved segment container (e.g. `"ts"`).
    pub segment_container: String,
}

/// Resolves an [`HlsStreamRequest`] into a concrete [`TranscodePlan`].
///
/// The seam over the un-ported `GetStreamingState` + `GetCommandLineArguments`
/// (see the module docs). Real implementations resolve the media source via
/// `ferrofin-core` and build args via the ported
/// [`EncodingHelper`](ferrofin_mediaencoding::EncodingHelper); unit tests supply a
/// deterministic fake so the orchestration around it is fully covered.
#[async_trait]
pub trait StreamStatePlanner: Send + Sync {
    /// Builds the transcode plan for `request`. `is_audio` selects the audio
    /// stream shape; `segment_id` is the first segment ffmpeg should emit (the
    /// `StartTimeTicks` seek target for a mid-stream segment request), or `None`
    /// for a playlist request that only needs the media path + runtime.
    async fn plan(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
        segment_id: Option<i32>,
    ) -> Result<TranscodePlan, ServiceError>;
}

/// The MIME type for an HLS playlist, matching Jellyfin's
/// `MimeTypes.GetMimeType("playlist.m3u8")`.
const HLS_PLAYLIST_MIME: &str = "application/x-mpegURL";

/// The ffmpeg program handed to the transcoder seam.
///
/// The [`StreamStatePlanner`] carries no encoder path, so the runtime relies on
/// `ffmpeg` being on `PATH` (matching the probe-only encoder default). The
/// Wave-8 planner resolves a concrete encoder path into the plan's args when a
/// non-default binary is configured.
const FFMPEG_PROGRAM: &str = "ffmpeg";

/// How many segments ahead of a running transcode's on-disk progress a request
/// may be before it is treated as a *seek* (evict the job and restart from the
/// requested segment) rather than a *read-ahead* (wait for the running job to
/// reach it). Small so a real timeline jump restarts promptly; non-zero so
/// normal read-ahead does not thrash the encoder with kill/restart churn.
///
/// ponytail: tuning knob — a candidate server setting if scrub/read-ahead
/// behaviour needs tuning per deployment.
const SEGMENT_WAIT_GAP: i32 = 2;

/// How many consecutive (re)started transcodes may die without producing their
/// requested segment before segment requests for that playlist fail fast
/// instead of spawning yet another ffmpeg.
///
/// A source that keeps killing ffmpeg (a flaky NFS mount serving truncated
/// reads, a file shorter than its metadata claims) otherwise turns every
/// client segment request into a fresh spawn: observed in production as a
/// kill/restart storm at ~2.4 ffmpeg spawns per second while the client
/// skip-walked the playlist. Three strikes tolerates a transient blip without
/// letting the storm run.
///
/// ponytail: tuning knob — candidate server setting alongside the cooldown.
const RESTART_FAILURE_LIMIT: u32 = 3;

/// How long segment requests for a playlist fail fast after
/// [`RESTART_FAILURE_LIMIT`] consecutive dead transcodes, before one new
/// attempt is allowed through (half-open). Long enough to stop a per-request
/// spawn storm; short enough that playback recovers on its own once the
/// storage heals.
///
/// ponytail: tuning knob — candidate server setting alongside the limit.
const RESTART_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);

/// The concrete [`HlsStreamManager`]: playlist generation + segment transcode
/// orchestration + legacy file resolution, over the injected runtime.
///
/// Generic over the [`StreamStatePlanner`] (the un-ported request→plan glue), the
/// [`SegmentTranscoder`] (the ffmpeg spawn seam), the [`EncodingOptionsProvider`]
/// (the generator's config accessor), and the [`SessionReporter`] (job teardown).
pub struct HlsStreamManagerImpl<P, T, C, S>
where
    P: StreamStatePlanner,
    T: SegmentTranscoder,
    C: EncodingOptionsProvider,
    S: SessionReporter,
{
    planner: P,
    transcoder: T,
    manager: Arc<TranscodeManagerImpl<S, FsFileCleaner>>,
    generator: Arc<DynamicHlsPlaylistGenerator<C>>,
    paths: Arc<dyn ServerApplicationPaths>,
    /// Per-playlist async locks serialising the "find/evict/start job" critical
    /// section of [`Self::resolve_dynamic_segment`]. Without it two concurrent
    /// seek requests for the same output could each spawn an ffmpeg writing the
    /// same `{stem}{n}.ts` files — a torn, corrupt segment. Port of Jellyfin's
    /// per-playlist `TranscodingLock`.
    segment_locks:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Per-playlist count of consecutive transcode (re)starts that died without
    /// producing their requested segment, plus the last failure time — the
    /// restart circuit breaker's state. Cleared the moment a started job
    /// delivers a segment.
    ///
    /// ponytail: entries for permanently-failing playlists linger like
    /// `segment_locks` entries do; add eviction only if either map ever
    /// measurably matters.
    restart_failures:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>>>,
}

impl<P, T, C, S> HlsStreamManagerImpl<P, T, C, S>
where
    P: StreamStatePlanner,
    T: SegmentTranscoder,
    C: EncodingOptionsProvider,
    S: SessionReporter,
{
    /// Assembles the manager from its collaborators.
    ///
    /// * `planner` — resolves a request into a [`TranscodePlan`].
    /// * `transcoder` — the ffmpeg segment-spawn seam.
    /// * `manager` — the live transcode-job registry (`start_ffmpeg` / kill).
    /// * `generator` — the `.m3u8` playlist generator.
    /// * `paths` — server paths (for the transcode cache directory).
    pub fn new(
        planner: P,
        transcoder: T,
        manager: Arc<TranscodeManagerImpl<S, FsFileCleaner>>,
        generator: Arc<DynamicHlsPlaylistGenerator<C>>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            planner,
            transcoder,
            manager,
            generator,
            paths,
            segment_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            restart_failures: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Whether the restart circuit breaker is open for playlist `key`: at least
    /// [`RESTART_FAILURE_LIMIT`] consecutive dead transcodes, the latest less
    /// than [`RESTART_FAILURE_COOLDOWN`] ago. Once the cooldown elapses the
    /// breaker lets one attempt through (half-open); a failure re-arms it, a
    /// success clears it.
    fn restart_breaker_open(&self, key: &str) -> bool {
        let map = self
            .restart_failures
            .lock()
            .expect("restart failures poisoned");
        map.get(key).is_some_and(|(count, last)| {
            *count >= RESTART_FAILURE_LIMIT && last.elapsed() < RESTART_FAILURE_COOLDOWN
        })
    }

    /// Records a dead transcode for playlist `key`, returning the consecutive
    /// failure count. Logs a warning when the count trips the breaker open.
    fn record_restart_failure(&self, key: &str) -> u32 {
        let mut map = self
            .restart_failures
            .lock()
            .expect("restart failures poisoned");
        let entry = map
            .entry(key.to_owned())
            .or_insert((0, std::time::Instant::now()));
        entry.0 += 1;
        entry.1 = std::time::Instant::now();
        if entry.0 == RESTART_FAILURE_LIMIT {
            tracing::warn!(
                playlist = key,
                failures = entry.0,
                cooldown_secs = RESTART_FAILURE_COOLDOWN.as_secs(),
                "transcode restart breaker open: segment requests will fail fast"
            );
        }
        entry.0
    }

    /// Clears the restart-failure record for playlist `key` (a started job
    /// delivered its segment).
    fn clear_restart_failures(&self, key: &str) {
        self.restart_failures
            .lock()
            .expect("restart failures poisoned")
            .remove(key);
    }

    /// Rewinds the breaker's last-failure time for `key` so tests can observe
    /// the half-open retry without waiting out the real cooldown.
    #[cfg(test)]
    fn force_cooldown_elapsed(&self, key: &str) {
        let mut map = self
            .restart_failures
            .lock()
            .expect("restart failures poisoned");
        if let Some((_, last)) = map.get_mut(key) {
            *last = std::time::Instant::now()
                .checked_sub(RESTART_FAILURE_COOLDOWN)
                .expect("cooldown rewind underflow");
        }
    }

    /// The per-playlist lock for `key`, creating it on first use. Held across the
    /// find/evict/start critical section so concurrent seeks serialise instead of
    /// racing two ffmpegs onto the same segment files.
    fn playlist_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.segment_locks.lock().expect("segment locks poisoned");
        Arc::clone(locks.entry(key.to_owned()).or_default())
    }

    /// Builds a variant/main playlist string for `request` from a plan.
    ///
    /// Port of `GetVariantPlaylistInternal`: builds a
    /// [`CreateMainPlaylistRequest`] from the plan and calls the generator. The
    /// endpoint prefix is Jellyfin's `"hls1/main/"`.
    async fn build_variant_playlist(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<String, ServiceError> {
        let plan = self.planner.plan(request, is_audio, None).await?;
        let media_source_id = request
            .media_source_id
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok());
        let create = CreateMainPlaylistRequest::new(
            media_source_id,
            plan.media_path,
            plan.segment_length_ms,
            plan.run_time_ticks,
            plan.segment_container,
            "hls1/main/",
            request.query_string.clone(),
            plan.is_remuxing_video,
        );
        // `HlsError` converts into `ServiceError::BackendSource` (HTTP 500),
        // preserving the typed playlist-generation failure as the cause.
        self.generator
            .create_main_playlist(&create)
            .map_err(ServiceError::from)
    }

    /// Starts (or reuses) the transcode for `request` and resolves segment
    /// `segment_id` on disk. Port of `GetDynamicSegment`.
    async fn resolve_dynamic_segment(
        &self,
        request: &HlsStreamRequest,
        segment_id: i32,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        use ferrofin_traits::media_encoding::TranscodeManager as _;

        // The fMP4 init segment (`#EXT-X-MAP` URI `.../-1.mp4`, negative index) is
        // written by the transcode itself via `-hls_fmp4_init_filename`, not a
        // normal segment. Serve it once a transcode has produced it.
        if segment_id < 0 {
            return self.resolve_init_segment(request, is_audio).await;
        }

        let plan = self
            .planner
            .plan(request, is_audio, Some(segment_id))
            .await?;
        let playlist_path = plan.playlist_path.clone();
        let playlist_key = playlist_path.to_string_lossy().into_owned();
        let ext = segment_extension(&plan.segment_container);
        let segment_path = segment_file(&playlist_path, segment_id, &ext);

        // Fast path: the segment already exists (a live job produced it) → mark
        // the consumer active (keep-alive) and serve it. Port of the
        // `File.Exists` try-1; the guard drop restarts the idle countdown.
        if segment_path.exists() {
            let _guard = self
                .manager
                .begin_request_guard(&playlist_key, TranscodingJobType::Hls);
            return Ok(served(&segment_path, &ext));
        }

        // Serialise the find/evict/start decision per playlist so two concurrent
        // requests can't each spawn an ffmpeg onto the same segment files.
        let lock = self.playlist_lock(&playlist_key);
        let _guard = lock.lock().await;

        // Another request may have produced the segment while we waited for the
        // lock — re-check before doing any work.
        if segment_path.exists() {
            return Ok(served(&segment_path, &ext));
        }

        // A transcode may already be running for this playlist. Only ONE ffmpeg
        // may own a given `{stem}{n}.ts` set: a second one started for a seek
        // re-numbers into the same files and tears the stream. So if the request
        // is only just ahead of the running job's on-disk progress, wait for it;
        // otherwise (a real seek, or the job is gone) evict the stale job and
        // (re)start from this segment. Port of `GetDynamicSegment`'s current-index
        // restart decision.
        if let Some(handle) = self
            .manager
            .get_transcoding_job_by_path(&playlist_key, TranscodingJobType::Hls)
            .await
            .ok()
            .flatten()
        {
            // `current` sees in-progress `.tmp` segments too, so a live job that
            // is still encoding its first segment reads as progress, not absence.
            let current = current_transcoding_index(&playlist_path, &ext);
            if should_wait_for_running_job(current, segment_id) {
                // The wait is an active consumer: the guard keeps the idle
                // reaper away and self-releases if the client disconnects.
                let _guard = self
                    .manager
                    .begin_request_guard(&playlist_key, TranscodingJobType::Hls);
                if self
                    .manager
                    .wait_for_segment(&handle, &playlist_path, segment_id)
                    .await
                    && segment_path.exists()
                {
                    self.clear_restart_failures(&playlist_key);
                    return Ok(served(&segment_path, &ext));
                }
            }
            // A seek (or the running job died mid-wait): drop the stale job before
            // restarting so the two don't write the same files. Keep its produced
            // segments (delete_files = false) — a later backward seek serves them
            // straight from disk via the fast path.
            self.manager.kill_and_remove(&handle, false).await;
        }

        // A source that keeps killing ffmpeg (flaky network storage, a file
        // shorter than its metadata claims) must not turn every client segment
        // request into a fresh spawn. After RESTART_FAILURE_LIMIT consecutive
        // dead transcodes, fail fast until the cooldown lets one retry through.
        if self.restart_breaker_open(&playlist_key) {
            return Err(ServiceError::backend(format!(
                "transcode for this stream died {RESTART_FAILURE_LIMIT}+ times in a row \
                 (source unreadable?); retrying after cooldown — see {}",
                playlist_path.with_extension("log").display()
            )));
        }

        // (Re)start the transcode from this segment and wait for it.
        let log_path = playlist_path.with_extension("log");
        let start = StartFfMpegRequest {
            program: FFMPEG_PROGRAM,
            state: &plan.state,
            output_path: &playlist_path,
            arguments: plan.arguments.clone(),
            log_path,
            working_dir: None,
        };
        let handle = match self.manager.start_ffmpeg(&self.transcoder, start).await {
            Ok(handle) => handle,
            Err(e) => {
                self.record_restart_failure(&playlist_key);
                return Err(ServiceError::backend(format!(
                    "failed to start transcode: {e}"
                )));
            }
        };

        // Consumer mark for the wait on the freshly-started job (see above).
        let _guard = self
            .manager
            .begin_request_guard(&playlist_key, TranscodingJobType::Hls);
        if self
            .manager
            .wait_for_segment(&handle, &playlist_path, segment_id)
            .await
            && segment_path.exists()
        {
            self.clear_restart_failures(&playlist_key);
            Ok(served(&segment_path, &ext))
        } else {
            // A 5xx, not a 404: HLS clients skip past a 404'd segment and walk
            // the whole playlist (each request spawning another doomed ffmpeg);
            // a server error makes them retry with backoff instead.
            let failures = self.record_restart_failure(&playlist_key);
            Err(ServiceError::backend(format!(
                "transcode exited before producing segment {segment_id} \
                 (consecutive failures: {failures}); see {}",
                playlist_path.with_extension("log").display()
            )))
        }
    }

    /// Serves the fMP4 init segment (`{stem}-1.mp4`) for `request`.
    ///
    /// The init header is written by ffmpeg (via `-hls_fmp4_init_filename`) when
    /// the transcode starts, before segment 0. So if it is not yet on disk,
    /// producing segment 0 starts the transcode — which writes the init — and we
    /// then serve it.
    async fn resolve_init_segment(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        let plan = self.planner.plan(request, is_audio, Some(0)).await?;
        let ext = segment_extension(&plan.segment_container);
        let init_path = init_segment_file(&plan.playlist_path, &ext);

        if !init_path.exists() {
            // Start the transcode at the RESUME segment, not segment 0. The fMP4
            // init header (its moov edit list) encodes the job's start offset, so
            // an init produced from segment 0 is incompatible with the seek-offset
            // segments a resuming client actually plays — the player maps that
            // media back to t≈0 and stalls on a black screen. Starting the job
            // where the client is about to play makes the cached init match those
            // segments. With no resume offset this is segment 0, as before.
            // Boxed to break the (depth-1) init→segment async recursion.
            //
            // ffmpeg writes the init header immediately, long before the first
            // segment completes — and a slow first segment (4K HEVC) can take
            // 30-60s, which times the client out and (for the cast receiver)
            // used to trigger a retry→kill→restart loop. So race the two: serve
            // the init the moment it lands on disk, letting the segment encode
            // continue in the background (C# waits on the init path itself,
            // DynamicHlsController.GetSegmentResult WaitForPath).
            let start = resume_segment_index(request.start_time_ticks, plan.segment_length_ms);
            let produce = Box::pin(self.resolve_dynamic_segment(request, start, is_audio));
            let init_appears = async {
                while !init_path.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            };
            tokio::select! {
                () = init_appears => {}
                result = produce => {
                    // The segment finished (or failed) before the init showed up.
                    result?;
                }
            }
        }

        if init_path.exists() {
            Ok(served(&init_path, &ext))
        } else {
            Err(ServiceError::NotFound(
                "fmp4 init segment did not materialise".to_owned(),
            ))
        }
    }

    /// The transcode cache directory (`GetTranscodePath`).
    fn transcode_dir(&self) -> PathBuf {
        PathBuf::from(self.paths.transcode_path())
    }

    /// Resolves `file_name` inside the transcode cache, guarding traversal.
    ///
    /// Port of `HlsSegmentController.ValidateTranscodePath`: the resolved path must
    /// stay inside the transcode folder. `require_m3u8` additionally requires a
    /// `.m3u8` extension (the legacy playlist route).
    fn resolve_in_transcode_dir(
        &self,
        file_name: &str,
        require_m3u8: bool,
    ) -> Result<PathBuf, ServiceError> {
        let dir = self.transcode_dir();
        let candidate = dir.join(file_name);
        // Reject any name that escapes the transcode dir (`..`, absolute, etc.).
        let escapes = Path::new(file_name).components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
        if escapes || !candidate.starts_with(&dir) {
            return Err(ServiceError::invalid_input("invalid segment"));
        }
        if require_m3u8
            && candidate
                .extension()
                .is_none_or(|e| !e.eq_ignore_ascii_case("m3u8"))
        {
            return Err(ServiceError::invalid_input("invalid segment"));
        }
        Ok(candidate)
    }
}

#[async_trait]
impl<P, T, C, S> HlsStreamManager for HlsStreamManagerImpl<P, T, C, S>
where
    P: StreamStatePlanner,
    T: SegmentTranscoder,
    C: EncodingOptionsProvider,
    S: SessionReporter,
{
    async fn master_playlist(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<String, ServiceError> {
        // The master playlist points at the single variant `main.m3u8`. Full
        // adaptive-bitrate master generation (`DynamicHlsHelper` — CODECS,
        // RESOLUTION, subtitle #EXT-X-MEDIA groups) is deferred; the
        // single-stream master lists one variant carrying the request query.
        //
        // BANDWIDTH is real, though: RFC 8216 requires a positive value, and the
        // old `BANDWIDTH=0` fed zero into client ABR bitrate math (NaN/Infinity
        // in hls.js; the Cast receiver's player also keys segment budgeting off
        // it). Negotiated output bitrate first, the source's probed bitrate as
        // the copy-stream fallback (upstream sums the output streams the same
        // way in DynamicHlsHelper.AppendPlaylist).
        let plan = self.planner.plan(request, is_audio, None).await?;
        let state = &plan.state;
        let output =
            state.output_video_bitrate.unwrap_or(0) + state.output_audio_bitrate.unwrap_or(0);
        let bandwidth = if output > 0 {
            output
        } else {
            state.media_source.bitrate.unwrap_or(0)
        }
        // A copy stream of an unprobed source still needs a positive value.
        .max(128_000);
        let variant_url = format!("main.m3u8{}", request.query_string);
        Ok(format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n\
             #EXT-X-STREAM-INF:BANDWIDTH={bandwidth},AVERAGE-BANDWIDTH={bandwidth}\n\
             {variant_url}\n"
        ))
    }

    async fn variant_playlist(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<String, ServiceError> {
        self.build_variant_playlist(request, is_audio).await
    }

    async fn live_playlist(&self, request: &HlsStreamRequest) -> Result<String, ServiceError> {
        // A live (open-ended) stream reuses the variant playlist shape.
        self.build_variant_playlist(request, false).await
    }

    async fn dynamic_segment(
        &self,
        request: &HlsStreamRequest,
        segment_id: i32,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        self.resolve_dynamic_segment(request, segment_id, is_audio)
            .await
    }

    async fn resolve_transcode_file(
        &self,
        file_name: &str,
        require_m3u8: bool,
    ) -> Result<ServedFile, ServiceError> {
        let path = self.resolve_in_transcode_dir(file_name, require_m3u8)?;
        if !path.exists() {
            return Err(ServiceError::invalid_input("invalid segment"));
        }
        // Refresh the owning job's keep-alive timer (OnTranscodeBeginRequest on
        // the playlist that owns this file).
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        Ok(served(&path, &ext))
    }

    async fn transcode_stream(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        // The progressive-transcode branch produces a single output file; reuse
        // the plan's output path and run the transcode to completion via the
        // segment-0 wait (a progressive job writes one growing file).
        let plan = self.planner.plan(request, is_audio, Some(0)).await?;
        let output = plan.state.output_file_path.clone();
        let log_path = Path::new(&output).with_extension("log");
        let start = StartFfMpegRequest {
            program: FFMPEG_PROGRAM,
            state: &plan.state,
            output_path: Path::new(&output),
            arguments: plan.arguments.clone(),
            log_path,
            working_dir: None,
        };
        self.manager
            .start_ffmpeg(&self.transcoder, start)
            .await
            .map_err(|e| ServiceError::backend(format!("failed to start transcode: {e}")))?;
        let ext = Path::new(&output)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        Ok(served(Path::new(&output), &ext))
    }

    async fn stop_encoding(&self, request: &HlsStreamRequest) -> Result<(), ServiceError> {
        use ferrofin_traits::media_encoding::TranscodeManager as _;
        let device = request.device_id.as_deref().unwrap_or_default();
        self.manager
            .kill_transcoding_jobs(device, request.play_session_id.as_deref(), true)
            .await
    }

    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError> {
        use ferrofin_traits::media_encoding::TranscodeManager as _;
        self.manager
            .ping_transcoding_job(play_session_id, is_user_paused)
            .await
    }
}

/// The segment file extension for `segment_container` (`.ts` default).
///
/// Mirrors `EncodingHelper.GetSegmentFileExtension` and the transcode manager's
/// own helper so the served path agrees with what ffmpeg wrote.
fn segment_extension(segment_container: &str) -> String {
    match segment_container.trim() {
        "mp4" => ".mp4".to_owned(),
        "" => ".ts".to_owned(),
        other => format!(".{other}"),
    }
}

/// The on-disk path of segment `index` for `playlist` (`GetSegmentPath`).
fn segment_file(playlist: &Path, index: i32, extension: &str) -> PathBuf {
    let folder = playlist.parent().unwrap_or_else(|| Path::new(""));
    let stem = playlist
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    folder.join(format!("{stem}{index}{extension}"))
}

/// The on-disk path of the fMP4 init segment (`{stem}-1{ext}`) for `playlist` —
/// the header ffmpeg writes via `-hls_fmp4_init_filename` and the `#EXT-X-MAP`
/// URI references.
fn init_segment_file(playlist: &Path, extension: &str) -> PathBuf {
    segment_file(playlist, -1, extension)
}

/// The segment index containing a resume offset of `start_time_ticks`, given a
/// segment length of `segment_length_ms`. Returns 0 (start) when the client is
/// not resuming or on any degenerate input. Used to start the fMP4 init
/// transcode where a resuming client will actually play, so the cached init
/// matches the seek-offset segments.
fn resume_segment_index(start_time_ticks: Option<i64>, segment_length_ms: i32) -> i32 {
    let ticks = start_time_ticks.unwrap_or(0);
    if ticks <= 0 || segment_length_ms <= 0 {
        return 0;
    }
    let segment_ticks = i64::from(segment_length_ms) * TICKS_PER_MILLISECOND;
    i32::try_from(ticks / segment_ticks).unwrap_or(0)
}

/// The highest segment index a transcode has written for `playlist` on disk.
///
/// Scans the output directory for `{stem}{n}{ext}` files and returns the max
/// `n` — the running job's progress front. `None` when nothing has been
/// produced yet. Port of `GetCurrentTranscodingIndex`; used to decide whether a
/// segment request is a read-ahead (wait) or a seek (evict + restart).
fn current_transcoding_index(playlist: &Path, ext: &str) -> Option<i32> {
    let folder = playlist.parent()?;
    let stem = playlist.file_stem()?.to_string_lossy().into_owned();
    let suffix = if ext.starts_with('.') {
        ext.to_owned()
    } else {
        format!(".{ext}")
    };
    std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            // `-hls_flags temp_file` writes the in-progress segment as
            // `<name>.tmp` and renames on completion; counting it keeps the
            // encoder's front visible during the (possibly long) first-segment
            // encode — otherwise a retry arriving in that window reads "no
            // progress" and kills a healthy job.
            let name = name.strip_suffix(".tmp").unwrap_or(name);
            name.strip_prefix(&stem)?
                .strip_suffix(&suffix)?
                .parse::<i32>()
                .ok()
        })
        .max()
}

/// Whether a segment request should wait on the running job rather than kill
/// and restart it (`GetDynamicSegment`'s current-index decision).
///
/// `None` (no segment on disk yet, not even a `.tmp`) means the job only just
/// spawned: wait — a client retry during the first-segment encode must not
/// kill a healthy job (the cast-receiver kill/restart storm). Otherwise wait
/// when the request is at or just ahead of the encoder's front (upstream also
/// waits on `segment_id == current`); behind it, or more than
/// [`SEGMENT_WAIT_GAP`] ahead, is a real seek → restart.
fn should_wait_for_running_job(current: Option<i32>, segment_id: i32) -> bool {
    match current {
        None => true,
        Some(c) => segment_id >= c && segment_id - c <= SEGMENT_WAIT_GAP,
    }
}

/// Builds a [`ServedFile`] for `path`, choosing the MIME type from `ext`.
fn served(path: &Path, ext: &str) -> ServedFile {
    let content_type = mime_for_extension(ext).to_owned();
    ServedFile {
        path: path.to_string_lossy().into_owned(),
        content_type,
    }
}

/// The MIME type for a served file extension.
///
/// Port of the `MimeTypes.GetMimeType` results the streaming controllers use for
/// the HLS artifacts (playlist + the common segment containers).
fn mime_for_extension(ext: &str) -> &'static str {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "m3u8" => HLS_PLAYLIST_MIME,
        "ts" => "video/mp2t",
        "mp4" | "m4s" => "video/mp4",
        "aac" => "audio/aac",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_mediaencoding::transcoding::{
        FakeScript, FakeSegmentTranscoder, NoopSessionReporter,
    };
    use ferrofin_mediaencoding::{BaseEncodingJobOptions, EncodingJobInfo};
    use ferrofin_model::configuration::EncodingOptions;
    use ferrofin_model::dto::MediaSourceInfo;
    use std::sync::Mutex;

    /// A fake [`ServerApplicationPaths`] with a fixed transcode directory.
    struct FakePaths {
        transcode: String,
    }

    impl ServerApplicationPaths for FakePaths {
        fn root_folder_path(&self) -> String {
            String::new()
        }
        fn default_user_views_path(&self) -> String {
            String::new()
        }
        fn people_path(&self) -> String {
            String::new()
        }
        fn genre_path(&self) -> String {
            String::new()
        }
        fn music_genre_path(&self) -> String {
            String::new()
        }
        fn studio_path(&self) -> String {
            String::new()
        }
        fn year_path(&self) -> String {
            String::new()
        }
        fn artists_path(&self) -> String {
            String::new()
        }
        fn user_configuration_directory_path(&self) -> String {
            String::new()
        }
        fn internal_metadata_path(&self) -> String {
            String::new()
        }
        fn program_data_path(&self) -> String {
            String::new()
        }
        fn web_path(&self) -> String {
            String::new()
        }
        fn data_path(&self) -> String {
            String::new()
        }
        fn image_cache_path(&self) -> String {
            String::new()
        }
        fn cache_path(&self) -> String {
            String::new()
        }
        fn log_directory_path(&self) -> String {
            String::new()
        }
        fn transcode_path(&self) -> String {
            self.transcode.clone()
        }
    }

    /// Records each `(is_audio, segment_id)` the fake planner was asked to plan.
    type PlanCalls = Arc<Mutex<Vec<(bool, Option<i32>)>>>;

    /// A fake planner writing its playlist under `dir`, with the recorded request.
    struct FakePlanner {
        dir: PathBuf,
        segment_container: String,
        requests: PlanCalls,
    }

    #[async_trait]
    impl StreamStatePlanner for FakePlanner {
        async fn plan(
            &self,
            _request: &HlsStreamRequest,
            is_audio: bool,
            segment_id: Option<i32>,
        ) -> Result<TranscodePlan, ServiceError> {
            self.requests.lock().unwrap().push((is_audio, segment_id));
            let playlist = self.dir.join("out.m3u8");
            let state = EncodingJobInfo {
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
                output_file_path: playlist.to_string_lossy().into_owned(),
                input_container: None,
                is_input_video: true,
                subtitle_delivery_method: ferrofin_model::dlna::SubtitleDeliveryMethod::Encode,
                run_time_ticks: Some(60 * 10_000_000),
                transcoding_type: TranscodingJobType::Hls,
                supported_video_codecs: Vec::new(),
                supported_audio_codecs: Vec::new(),
                segment_length_secs: 6,
                wait_for_path: None,
                segment_container: Some(self.segment_container.clone()),
                play_session_id: Some("sess".to_owned()),
                device_id: Some("dev".to_owned()),
            };
            Ok(TranscodePlan {
                state,
                playlist_path: playlist,
                arguments: vec!["-i".to_owned(), "in.mkv".to_owned()],
                media_path: "/media/in.mkv".to_owned(),
                run_time_ticks: 60 * 10_000_000,
                segment_length_ms: 6000,
                is_remuxing_video: false,
                segment_container: self.segment_container.clone(),
            })
        }
    }

    fn generator()
    -> Arc<DynamicHlsPlaylistGenerator<Box<dyn Fn() -> EncodingOptions + Send + Sync>>> {
        let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> =
            Box::new(EncodingOptions::default);
        Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()))
    }

    type Mgr = HlsStreamManagerImpl<
        FakePlanner,
        FakeSegmentTranscoder,
        Box<dyn Fn() -> EncodingOptions + Send + Sync>,
        NoopSessionReporter,
    >;

    fn manager_full(
        dir: &Path,
        script: FakeScript,
        container: &str,
    ) -> (Mgr, PlanCalls, FakeSegmentTranscoder) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let planner = FakePlanner {
            dir: dir.to_path_buf(),
            segment_container: container.to_owned(),
            requests: requests.clone(),
        };
        let transcoder = FakeSegmentTranscoder::new(script);
        let spawns = transcoder.clone();
        let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
        let paths = Arc::new(FakePaths {
            transcode: dir.to_string_lossy().into_owned(),
        });
        let mgr = HlsStreamManagerImpl::new(planner, transcoder, manager, generator(), paths);
        (mgr, requests, spawns)
    }

    fn manager_with(dir: &Path, script: FakeScript, container: &str) -> (Mgr, PlanCalls) {
        let (mgr, requests, _) = manager_full(dir, script, container);
        (mgr, requests)
    }

    fn req() -> HlsStreamRequest {
        HlsStreamRequest {
            item_id: uuid::Uuid::from_u128(1),
            device_id: Some("dev".to_owned()),
            play_session_id: Some("sess".to_owned()),
            segment_container: Some("ts".to_owned()),
            query_string: "?deviceId=dev".to_owned(),
            ..HlsStreamRequest::default()
        }
    }

    #[tokio::test]
    async fn master_playlist_lists_the_variant_with_query() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let pl = mgr.master_playlist(&req(), false).await.unwrap();
        assert!(pl.contains("#EXTM3U"));
        assert!(pl.contains("main.m3u8?deviceId=dev"));
    }

    #[tokio::test]
    async fn variant_playlist_is_generated() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, recorded) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let pl = mgr.variant_playlist(&req(), false).await.unwrap();
        assert!(pl.starts_with("#EXTM3U"));
        // The planner was asked for a playlist (segment_id None), video.
        assert_eq!(recorded.lock().unwrap()[0], (false, None));
    }

    #[tokio::test]
    async fn live_playlist_uses_the_variant_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let pl = mgr.live_playlist(&req()).await.unwrap();
        assert!(pl.starts_with("#EXTM3U"));
    }

    #[tokio::test]
    async fn dynamic_segment_starts_transcode_and_serves_segment() {
        let tmp = tempfile::tempdir().unwrap();
        // The fake writes out0.ts + the playlist synchronously on spawn.
        let script = FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        };
        let (mgr, _) = manager_with(tmp.path(), script, "ts");
        let served = mgr.dynamic_segment(&req(), 0, false).await.unwrap();
        assert!(served.path.ends_with("out0.ts"));
        assert_eq!(served.content_type, "video/mp2t");
    }

    #[tokio::test]
    async fn dead_transcode_is_a_backend_error_not_a_404() {
        let tmp = tempfile::tempdir().unwrap();
        // ffmpeg exits 0 without writing the requested segment — a truncated or
        // unreadable source (e.g. a flaky NFS mount). The client must get a
        // 5xx it retries, not a 404 it skips past.
        let script = FakeScript {
            extra_files: vec!["out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        };
        let (mgr, _) = manager_with(tmp.path(), script, "ts");
        let err = mgr.dynamic_segment(&req(), 5, false).await.unwrap_err();
        assert!(matches!(err, ServiceError::Backend(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn restart_breaker_stops_the_spawn_storm_and_half_opens() {
        let tmp = tempfile::tempdir().unwrap();
        // Every spawned "ffmpeg" dies without producing its segment.
        let script = FakeScript {
            extra_files: vec!["out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        };
        let (mgr, _, spawns) = manager_full(tmp.path(), script, "ts");

        // Each request up to the limit spawns (and loses) one transcode.
        for _ in 0..RESTART_FAILURE_LIMIT {
            let err = mgr.dynamic_segment(&req(), 5, false).await.unwrap_err();
            assert!(matches!(err, ServiceError::Backend(_)));
        }
        let spawned = spawns.requests.lock().unwrap().len();
        assert_eq!(spawned, RESTART_FAILURE_LIMIT as usize);

        // Breaker open: the next request fails fast with NO new spawn.
        let err = mgr.dynamic_segment(&req(), 5, false).await.unwrap_err();
        assert!(matches!(err, ServiceError::Backend(_)));
        assert_eq!(spawns.requests.lock().unwrap().len(), spawned);

        // After the cooldown, exactly one retry is let through (half-open);
        // its failure re-arms the breaker.
        let playlist_key = tmp.path().join("out.m3u8").to_string_lossy().into_owned();
        mgr.force_cooldown_elapsed(&playlist_key);
        let _ = mgr.dynamic_segment(&req(), 5, false).await.unwrap_err();
        assert_eq!(spawns.requests.lock().unwrap().len(), spawned + 1);
        let _ = mgr.dynamic_segment(&req(), 5, false).await.unwrap_err();
        assert_eq!(spawns.requests.lock().unwrap().len(), spawned + 1);
    }

    #[tokio::test]
    async fn dynamic_segment_fast_path_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create the segment so the fast path (no spawn) is taken.
        std::fs::write(tmp.path().join("out0.ts"), b"seg").unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let served = mgr.dynamic_segment(&req(), 0, false).await.unwrap();
        assert!(served.path.ends_with("out0.ts"));
    }

    #[test]
    fn resume_segment_index_maps_offset_to_segment() {
        // 6s segments (6000 ms). 60s resume → segment 10.
        assert_eq!(resume_segment_index(Some(60 * 10_000_000), 6000), 10);
        // No resume / zero / negative / bad length → segment 0.
        assert_eq!(resume_segment_index(None, 6000), 0);
        assert_eq!(resume_segment_index(Some(0), 6000), 0);
        assert_eq!(resume_segment_index(Some(-5), 6000), 0);
        assert_eq!(resume_segment_index(Some(60 * 10_000_000), 0), 0);
    }

    #[tokio::test]
    async fn init_segment_starts_transcode_at_resume_offset() {
        // A resuming client fetches the fMP4 init first; it must start the
        // transcode at the resume segment (not 0) so the cached init matches the
        // seek-offset segments it then plays. 60s / 6s segments → segment 10.
        let tmp = tempfile::tempdir().unwrap();
        let script = FakeScript {
            // The fake writes the init header + the resume segment on spawn.
            segment_files: vec!["out10.mp4".to_owned()],
            extra_files: vec!["out-1.mp4".to_owned(), "out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        };
        let (mgr, recorded) = manager_with(tmp.path(), script, "mp4");
        let resume_req = HlsStreamRequest {
            segment_container: Some("mp4".to_owned()),
            start_time_ticks: Some(60 * 10_000_000),
            ..req()
        };
        // Segment id -1 routes to the init serve.
        let served = mgr.dynamic_segment(&resume_req, -1, false).await.unwrap();
        assert!(served.path.ends_with("out-1.mp4"));
        // The init serve started the transcode at the resume segment (10), not 0.
        let calls = recorded.lock().unwrap();
        assert!(
            calls.contains(&(false, Some(10))),
            "expected a plan at resume segment 10, got {calls:?}"
        );
    }

    #[tokio::test]
    async fn resolve_transcode_file_serves_inside_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("abc0.ts"), b"seg").unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let served = mgr.resolve_transcode_file("abc0.ts", false).await.unwrap();
        assert!(served.path.ends_with("abc0.ts"));
        assert_eq!(served.content_type, "video/mp2t");
    }

    #[tokio::test]
    async fn resolve_transcode_file_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let err = mgr
            .resolve_transcode_file("../etc/passwd", false)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn resolve_transcode_file_requires_m3u8_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("abc.ts"), b"seg").unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        // A .ts file fails the m3u8 requirement.
        let err = mgr
            .resolve_transcode_file("abc.ts", true)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn resolve_transcode_playlist_serves_m3u8() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pl.m3u8"), b"#EXTM3U").unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let served = mgr.resolve_transcode_file("pl.m3u8", true).await.unwrap();
        assert_eq!(served.content_type, "application/x-mpegURL");
    }

    #[tokio::test]
    async fn missing_transcode_file_is_invalid_input() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let err = mgr
            .resolve_transcode_file("nope.ts", false)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn stop_encoding_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        assert!(mgr.stop_encoding(&req()).await.is_ok());
    }

    #[tokio::test]
    async fn ping_transcoding_job_is_ok_and_validates_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        // Pinging a session with no live job is a successful no-op.
        assert!(mgr.ping_transcoding_job("play-1", Some(true)).await.is_ok());
        // An empty play-session id is rejected.
        assert!(matches!(
            mgr.ping_transcoding_job("", None).await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[test]
    fn segment_extension_and_mime_mapping() {
        assert_eq!(segment_extension("ts"), ".ts");
        assert_eq!(segment_extension("mp4"), ".mp4");
        assert_eq!(segment_extension(""), ".ts");
        assert_eq!(mime_for_extension(".m3u8"), "application/x-mpegURL");
        assert_eq!(mime_for_extension("ts"), "video/mp2t");
        assert_eq!(mime_for_extension(".aac"), "audio/aac");
        assert_eq!(mime_for_extension(".mp3"), "audio/mpeg");
        assert_eq!(mime_for_extension(".xyz"), "application/octet-stream");
    }

    #[test]
    fn segment_file_mirrors_get_segment_path() {
        let p = segment_file(Path::new("/c/out.m3u8"), 4, ".ts");
        assert_eq!(p, Path::new("/c/out4.ts"));
    }

    #[test]
    fn current_transcoding_index_is_the_max_produced_segment() {
        let dir = tempfile::tempdir().unwrap();
        let playlist = dir.path().join("abc123.m3u8");
        // Nothing produced yet.
        assert_eq!(current_transcoding_index(&playlist, ".ts"), None);
        // A running job wrote segments 0,1,2 plus its playlist + a foreign file.
        for n in [0, 1, 2] {
            std::fs::write(dir.path().join(format!("abc123{n}.ts")), b"x").unwrap();
        }
        std::fs::write(&playlist, b"#EXTM3U").unwrap();
        std::fs::write(dir.path().join("other9.ts"), b"x").unwrap(); // different stem
        assert_eq!(current_transcoding_index(&playlist, ".ts"), Some(2));
        // After a seek restart wrote 20,21, the front is the highest index.
        std::fs::write(dir.path().join("abc12320.ts"), b"x").unwrap();
        std::fs::write(dir.path().join("abc12321.ts"), b"x").unwrap();
        assert_eq!(current_transcoding_index(&playlist, ".ts"), Some(21));
        // An in-progress `temp_file` segment counts as the encoder's front —
        // during the first-segment encode the job must not read as "no progress".
        std::fs::write(dir.path().join("abc12322.ts.tmp"), b"x").unwrap();
        assert_eq!(current_transcoding_index(&playlist, ".ts"), Some(22));
    }

    #[test]
    fn wait_vs_restart_decision_matches_upstream() {
        use super::should_wait_for_running_job;
        // Job just spawned, nothing on disk (not even a .tmp): wait, never kill
        // — a retry in the first-segment window used to kill/restart-loop the
        // cast receiver's playback.
        assert!(should_wait_for_running_job(None, 0));
        assert!(should_wait_for_running_job(None, 5));
        // At the encoder's front: wait (upstream waits on ==, Ferrofin used to
        // restart here).
        assert!(should_wait_for_running_job(Some(3), 3));
        // Just ahead (within the read-ahead gap): wait.
        assert!(should_wait_for_running_job(Some(3), 5));
        // Behind the front (backward seek) or far ahead (forward seek): restart.
        assert!(!should_wait_for_running_job(Some(3), 2));
        assert!(!should_wait_for_running_job(Some(3), 6));
    }
}
