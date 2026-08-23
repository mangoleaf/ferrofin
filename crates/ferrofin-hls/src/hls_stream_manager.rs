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
use ferrofin_mediaencoding::keyed_locks::KeyedLocks;
use ferrofin_mediaencoding::transcoding::manager::StartFfMpegRequest;
use ferrofin_mediaencoding::transcoding::segment_transcoder::SegmentTranscoder;
use ferrofin_mediaencoding::transcoding::{FsFileCleaner, SessionReporter};
use ferrofin_mediaencoding::{EncodingJobInfo, TranscodeManagerImpl};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{
    HlsStreamManager, HlsStreamRequest, ServedFile, TranscodingJobType,
};
use ferrofin_traits::system::ServerApplicationPaths;
use ferrofin_traits::trickplay::TrickplayManager;

use crate::create_main_playlist_request::CreateMainPlaylistRequest;
use crate::dynamic_hls_playlist_generator::{
    DynamicHlsPlaylistGenerator, EncodingOptionsProvider, TICKS_PER_MILLISECOND,
};
use crate::master_playlist::{MasterPlaylistContext, TrickplayResolution, build_master_playlist};

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
    /// A snapshot of the server's encoding options at plan time — the master
    /// playlist reads `AllowHevcEncoding`/`AllowAv1Encoding` for its SDR
    /// compatibility variants (`GetMasterPlaylistInternal`'s
    /// `_serverConfigurationManager.GetEncodingOptions()`).
    pub encoding_options: EncodingOptions,
    /// The minimum segment count a live (`live.m3u8`) request waits for before
    /// serving the playlist ffmpeg wrote. Port of `StreamState.MinSegments`:
    /// the request's `MinSegments`, else 2 for segments of ten seconds or
    /// longer, else 3.
    pub min_segments: i32,
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
const HLS_PLAYLIST_MIME: &str = "application/vnd.apple.mpegurl";

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
    /// Keyed by the playlist's output path, which carries a per-play-session
    /// id, so the key space grows with every playback — hence [`KeyedLocks`],
    /// which forgets a key once nobody holds it.
    segment_locks: Arc<KeyedLocks>,
    /// Per-playlist count of consecutive transcode (re)starts that died without
    /// producing their requested segment, plus the last failure time — the
    /// restart circuit breaker's state. Cleared the moment a started job
    /// delivers a segment.
    ///
    /// Keyed by the same per-session playlist path, so a record whose cooldown
    /// has long expired can never influence a decision again and is swept on the
    /// next write — otherwise every playback that ever failed would be
    /// remembered for the life of the process.
    restart_failures:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>>>,
    /// The trickplay store the master playlist lists image playlists from
    /// (`DynamicHlsHelper`'s `ITrickplayManager.GetTrickplayResolutions`).
    /// `None` (the default, and every pre-seam test constructor) lists none.
    trickplay: Option<Arc<dyn TrickplayManager>>,
}

impl<P, T, C, S> HlsStreamManagerImpl<P, T, C, S>
where
    P: StreamStatePlanner,
    T: SegmentTranscoder,
    C: EncodingOptionsProvider,
    S: SessionReporter,
{
    /// Wires the trickplay store so master playlists advertise the
    /// `#EXT-X-IMAGE-STREAM-INF` tile playlists (the composition root calls
    /// this; without it the master lists no trickplay entries).
    #[must_use]
    pub fn with_trickplay(mut self, trickplay: Arc<dyn TrickplayManager>) -> Self {
        self.trickplay = Some(trickplay);
        self
    }

    /// The trickplay resolutions for the request's media source, for the
    /// master playlist. Port of the `Guid.Parse(state.Request.MediaSourceId)`
    /// → `GetTrickplayResolutions` lookup; an absent/unparsable id or a store
    /// error yields none (upstream would 500 on the former — not ported).
    async fn trickplay_resolutions(&self, request: &HlsStreamRequest) -> Vec<TrickplayResolution> {
        let Some(trickplay) = self.trickplay.as_ref() else {
            return Vec::new();
        };
        let Some(source_id) = request
            .media_source_id
            .as_deref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            return Vec::new();
        };
        match trickplay.get_trickplay_resolutions(source_id).await {
            Ok(map) => map
                .into_iter()
                .map(|(width, info)| TrickplayResolution {
                    width,
                    height: info.height,
                    bandwidth: info.bandwidth,
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, %source_id, "failed to load trickplay resolutions for the master playlist");
                Vec::new()
            }
        }
    }
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
            segment_locks: Arc::new(KeyedLocks::new()),
            restart_failures: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            trickplay: None,
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
        // A record older than the cooldown can no longer open the breaker, so it
        // is dead weight; sweeping here keeps the map to the playlists actually
        // failing rather than every playlist that ever failed.
        map.retain(|k, (_, last)| k == key || last.elapsed() < RESTART_FAILURE_COOLDOWN);
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
        self.segment_locks.get(key)
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

    /// Serves `segment_path` if it is already on disk, marking the owning
    /// playlist's transcode as actively consumed (the `OnTranscodeBeginRequest`
    /// keep-alive) so the idle reaper leaves it alone. `None` when the segment
    /// is not there yet.
    ///
    /// Both existence checks in [`Self::resolve_dynamic_segment`] — the
    /// pre-lock fast path and the post-lock re-check — go through here, so
    /// neither can serve a client without restamping the job.
    fn serve_if_present(
        &self,
        playlist_key: &str,
        segment_path: &Path,
        ext: &str,
    ) -> Option<ServedFile> {
        if !segment_path.exists() {
            return None;
        }
        // The guard's drop restarts the idle countdown from now.
        let _guard = self
            .manager
            .begin_request_guard(playlist_key, TranscodingJobType::Hls);
        Some(served(segment_path, ext))
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
        if let Some(file) = self.serve_if_present(&playlist_key, &segment_path, &ext) {
            return Ok(file);
        }

        // Serialise the find/evict/start decision per playlist so two concurrent
        // requests can't each spawn an ffmpeg onto the same segment files.
        let lock = self.playlist_lock(&playlist_key);
        let _guard = lock.lock().await;

        // Another request may have produced the segment while we waited for the
        // lock — re-check before doing any work. Through the same helper: this
        // branch serves a real client just like the fast path above, and when
        // it open-coded the check without the keep-alive it left
        // `last_activity` stale and the idle reaper free to kill a job that was
        // actively being consumed.
        if let Some(file) = self.serve_if_present(&playlist_key, &segment_path, &ext) {
            return Ok(file);
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
    /// ffmpeg's hls muxer *creates* the init file empty when it writes the
    /// stream header, but only fills it at the first segment boundary — in the
    /// same flush that writes segment 0's data, immediately before segment 0's
    /// `temp_file` rename (measured on a real ffmpeg: created at 38ms, 0 bytes
    /// until 2540ms, segment 0 renamed at 2543ms). So "the init exists" proves
    /// nothing, and "the start segment exists" proves the init is complete and
    /// closed. This gates on the latter — upstream's `WaitForPath = segment 0`
    /// for `segmentId == -1` — and never sleeps on a size-stability poll.
    async fn resolve_init_segment(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        use ferrofin_traits::media_encoding::TranscodeManager as _;

        let plan = self.planner.plan(request, is_audio, Some(0)).await?;
        let ext = segment_extension(&plan.segment_container);
        let init_path = init_segment_file(&plan.playlist_path, &ext);

        // Start the transcode at the RESUME segment, not segment 0. The fMP4
        // init header (its moov edit list) encodes the job's start offset, so
        // an init produced from segment 0 is incompatible with the seek-offset
        // segments a resuming client actually plays. With no resume offset this
        // is segment 0, as before.
        //
        // Only the `-ss`/`-start_number` arguments differ with the start
        // segment — the output id, playlist path and container do not — so the
        // segment-0 plan above already answers everything up to here, and a
        // non-resuming client (`start == 0`) needs no second plan at all. That
        // second plan is a full media-source resolution (3 DB round trips) plus
        // an arg rebuild, paid on every fMP4 playback start.
        let start = resume_segment_index(request.start_time_ticks, plan.segment_length_ms);
        let plan = if start == 0 {
            plan
        } else {
            self.planner.plan(request, is_audio, Some(start)).await?
        };
        let start_segment = segment_file(&plan.playlist_path, start, &ext);

        if init_is_complete(&init_path, &start_segment) {
            // A completed/earlier job left both — complete by construction.
            return Ok(served(&init_path, &ext));
        }

        let playlist_key = plan.playlist_path.to_string_lossy().into_owned();
        let lock = self.playlist_lock(&playlist_key);
        let _guard = lock.lock().await;

        if !start_segment.exists() {
            if let Some(handle) = self
                .manager
                .get_transcoding_job_by_path(&playlist_key, TranscodingJobType::Hls)
                .await
                .ok()
                .flatten()
            {
                // A live job owns the files (hls.js requests the init and the
                // first segment concurrently, and the segment request may have
                // spawned first); wait for its start segment — the consumer
                // guard keeps the idle reaper away.
                let _rg = self
                    .manager
                    .begin_request_guard(&playlist_key, TranscodingJobType::Hls);
                let _ = self
                    .manager
                    .wait_for_segment(&handle, &plan.playlist_path, start)
                    .await;
            } else {
                if self.restart_breaker_open(&playlist_key) {
                    return Err(ServiceError::backend(format!(
                        "transcode for this stream died {RESTART_FAILURE_LIMIT}+ times in a row \
                         (source unreadable?); retrying after cooldown — see {}",
                        plan.playlist_path.with_extension("log").display()
                    )));
                }
                // NEVER a select!/cancellation over the start: a cancelled
                // spawn can orphan an unregistered ffmpeg, and the next segment
                // request then starts a second writer onto the same segment
                // files — torn, unparsable fragments (the fragParsingError
                // regression).
                let mut state = plan.state.clone();
                state.wait_for_path = Some(start_segment.clone());
                let log_path = plan.playlist_path.with_extension("log");
                let start_request = StartFfMpegRequest {
                    program: FFMPEG_PROGRAM,
                    state: &state,
                    output_path: &plan.playlist_path,
                    arguments: plan.arguments.clone(),
                    log_path,
                    working_dir: None,
                };
                if let Err(e) = self
                    .manager
                    .start_ffmpeg(&self.transcoder, start_request)
                    .await
                {
                    self.record_restart_failure(&playlist_key);
                    return Err(ServiceError::backend(format!(
                        "failed to start transcode: {e}"
                    )));
                }
                self.clear_restart_failures(&playlist_key);
            }
        }

        if init_is_complete(&init_path, &start_segment) {
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
        // `GetMasterPlaylistInternal`: resolve the state (the plan), then
        // assemble the playlist from it. The trickplay lookup is the one async
        // collaborator the pure builder cannot own; it is skipped exactly when
        // upstream skips it (a live stream, or `enableTrickplay=false`).
        let plan = self.planner.plan(request, is_audio, None).await?;
        // `state.VideoRequest?.EnableTrickplay ?? false`: only a video-route
        // request lists trickplay — the audio master's DTO has no such flag.
        let trickplay_resolutions = if !plan.state.is_segmented_live_stream()
            && plan.state.is_input_video
            && request.enable_trickplay
        {
            self.trickplay_resolutions(request).await
        } else {
            Vec::new()
        };
        let ctx = MasterPlaylistContext {
            trickplay_resolutions,
        };
        Ok(build_master_playlist(&plan, request, &ctx))
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
        // Boxed: the segment resolver's future grew past the large-future lint
        // once the init path stopped nesting inside it.
        Box::pin(self.resolve_dynamic_segment(request, segment_id, is_audio)).await
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
        // the playlist that owns this file). This comment used to stand alone
        // over code that never made the call: a client driving playback through
        // the legacy `hls` routes never touched `last_activity`, so unless it
        // also sent playback-progress pings the idle reaper killed its ffmpeg
        // 60s in — mid-playback. Upstream `HlsSegmentController.GetFileResult`
        // does exactly this for the legacy video-segment and playlist routes.
        // The legacy *audio* route serves without it upstream; sharing the
        // refresh here is a deliberate, response-invisible superset (it can
        // only stop a reap of a job a client is actively consuming).
        let _rg = self
            .manager
            .begin_request_guard_for_file(&path, TranscodingJobType::Hls);
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

/// Whether the fMP4 init at `init` is complete: ffmpeg fills it in the flush
/// that finishes `start_segment`, so the start segment's (renamed, final) file
/// existing implies the init was written and closed; the size check is only a
/// guard against a stale empty file from a job that died at its header.
fn init_is_complete(init: &Path, start_segment: &Path) -> bool {
    start_segment.exists() && std::fs::metadata(init).is_ok_and(|m| m.len() > 0)
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
    use ferrofin_mediaencoding::TranscodeDisplayNames;
    use ferrofin_mediaencoding::transcoding::{
        FakeScript, FakeSegmentTranscoder, NoopSessionReporter,
    };
    use ferrofin_mediaencoding::{BaseEncodingJobOptions, EncodingJobInfo};
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
                encoding_options: EncodingOptions::default(),
                min_segments: 3,
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
        assert!(pl.starts_with("#EXTM3U\n#EXT-X-STREAM-INF:"), "got: {pl}");
        // Upstream's master never carries a version tag.
        assert!(!pl.contains("#EXT-X-VERSION"), "got: {pl}");
        // The FakePlanner supplies no output bitrates, and the faithful
        // `totalBitrate` is the plain sum — no floor, no source fallback.
        assert!(pl.contains("BANDWIDTH=0,AVERAGE-BANDWIDTH=0"), "got: {pl}");
        // `segment_container` is set on the request but absent from the query,
        // so the universal-audio `&SegmentContainer=` append fires.
        assert!(
            pl.ends_with("\nmain.m3u8?deviceId=dev&SegmentContainer=ts\n"),
            "got: {pl}"
        );
    }

    /// Without a wired trickplay store the master lists no image playlists;
    /// with one, the store's resolutions for the media source appear.
    /// A trickplay store reporting one 320-wide resolution for every item.
    struct FakeTrickplay;

    #[async_trait]
    impl TrickplayManager for FakeTrickplay {
        async fn refresh_trickplay_data(
            &self,
            _item_id: uuid::Uuid,
            _replace: bool,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_trickplay_resolutions(
            &self,
            item_id: uuid::Uuid,
        ) -> Result<
            std::collections::HashMap<i32, ferrofin_db::entities::playback::TrickplayInfoEntity>,
            ServiceError,
        > {
            let info = ferrofin_db::entities::playback::TrickplayInfoEntity {
                item_id: item_id.simple().to_string(),
                width: 320,
                height: 180,
                bandwidth: 99_000,
                interval: 10_000,
                thumbnail_count: 1,
                tile_height: 1,
                tile_width: 1,
            };
            Ok(std::collections::HashMap::from([(320, info)]))
        }
        async fn get_trickplay_items(
            &self,
            _limit: i32,
            _offset: i32,
        ) -> Result<Vec<ferrofin_db::entities::playback::TrickplayInfoEntity>, ServiceError>
        {
            Ok(Vec::new())
        }
        async fn save_trickplay_info(
            &self,
            _info: &ferrofin_db::entities::playback::TrickplayInfoEntity,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_trickplay_data(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_trickplay_manifest(
            &self,
            _item_id: uuid::Uuid,
        ) -> Result<
            std::collections::HashMap<
                String,
                std::collections::HashMap<
                    i32,
                    ferrofin_db::entities::playback::TrickplayInfoEntity,
                >,
            >,
            ServiceError,
        > {
            Ok(std::collections::HashMap::new())
        }
        async fn get_hls_playlist(
            &self,
            _item_id: uuid::Uuid,
            _width: i32,
            _api_key: Option<&str>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn get_trickplay_tile_path(
            &self,
            _item_id: uuid::Uuid,
            _width: i32,
            _index: i32,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn master_playlist_lists_trickplay_when_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");
        let request = HlsStreamRequest {
            media_source_id: Some(uuid::Uuid::from_u128(9).simple().to_string()),
            api_key: Some("tok".to_owned()),
            ..req()
        };
        let pl = mgr.master_playlist(&request, false).await.unwrap();
        assert!(!pl.contains("IMAGE-STREAM"), "unwired: {pl}");

        let mgr = mgr.with_trickplay(Arc::new(FakeTrickplay));
        let pl = mgr.master_playlist(&request, false).await.unwrap();
        assert!(
            pl.contains(
                "#EXT-X-IMAGE-STREAM-INF:BANDWIDTH=99000,RESOLUTION=320x180,CODECS=\"jpeg\",URI=\"Trickplay/320/tiles.m3u8?MediaSourceId=00000000000000000000000000000009&ApiKey=tok\"\n"
            ),
            "wired: {pl}"
        );
        // `enableTrickplay=false` suppresses the lookup and the lines.
        let pl = mgr
            .master_playlist(
                &HlsStreamRequest {
                    enable_trickplay: false,
                    ..request
                },
                false,
            )
            .await
            .unwrap();
        assert!(!pl.contains("IMAGE-STREAM"), "disabled: {pl}");
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

    #[test]
    fn restart_failure_records_do_not_accumulate_across_playback_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");

        // Each playback session has its own output path, so every failing
        // stream mints a fresh key. Rewinding each record past the cooldown is
        // what a real long-lived server sees: sessions that failed hours ago.
        for i in 0..500 {
            let key = format!("/cache/session-{i}/out.m3u8");
            mgr.record_restart_failure(&key);
            mgr.force_cooldown_elapsed(&key);
        }
        let live = mgr.restart_failures.lock().unwrap().len();
        assert_eq!(
            live, 1,
            "records past their cooldown can no longer open the breaker and must \
             not be retained; found {live} of 500"
        );
    }

    #[test]
    fn a_playlist_still_failing_keeps_its_record_while_others_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");

        // Trip the breaker for one playlist, then churn unrelated stale keys
        // past it. The sweep must not disarm a breaker that is still live.
        for _ in 0..RESTART_FAILURE_LIMIT {
            mgr.record_restart_failure("/cache/hot/out.m3u8");
        }
        assert!(mgr.restart_breaker_open("/cache/hot/out.m3u8"));
        for i in 0..50 {
            let key = format!("/cache/cold-{i}/out.m3u8");
            mgr.record_restart_failure(&key);
            mgr.force_cooldown_elapsed(&key);
        }
        assert!(
            mgr.restart_breaker_open("/cache/hot/out.m3u8"),
            "sweeping stale records must not reset a live breaker"
        );
    }

    #[tokio::test]
    async fn segment_locks_do_not_accumulate_across_playback_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _) = manager_with(tmp.path(), FakeScript::default(), "ts");

        // Each lock handle is dropped at the end of the iteration, exactly as a
        // completed segment request releases it.
        for i in 0..500 {
            let _lock = mgr.playlist_lock(&format!("/cache/session-{i}/out.m3u8"));
        }
        assert_eq!(
            mgr.segment_locks.len(),
            1,
            "a per-session playlist lock must be forgotten once nobody holds it"
        );
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

    #[tokio::test]
    async fn serving_an_existing_segment_keeps_the_job_alive() {
        // Serving a segment straight off disk is an active consumer: it must
        // restamp the owning job, or the idle reaper kills the ffmpeg out from
        // under a client that is streaming along happily.
        let tmp = tempfile::tempdir().unwrap();
        let script = FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        };
        let (mgr, _) = manager_with(tmp.path(), script, "ts");
        mgr.dynamic_segment(&req(), 0, false).await.unwrap();
        let key = tmp.path().join("out.m3u8").to_string_lossy().into_owned();

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let before = mgr
            .manager
            .millis_since_activity(&key, TranscodingJobType::Hls)
            .expect("job registered");
        assert!(before >= 50, "idle clock should have advanced: {before}ms");

        // Segment 0 is on disk now, so this re-serves it off the fast path.
        mgr.dynamic_segment(&req(), 0, false).await.unwrap();

        let after = mgr
            .manager
            .millis_since_activity(&key, TranscodingJobType::Hls)
            .expect("job still registered");
        assert!(
            after < before,
            "serving an on-disk segment must restamp the job: {before}ms -> {after}ms"
        );
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
    async fn init_segment_without_a_resume_offset_plans_once() {
        // Only `-ss`/`-start_number` vary with the start segment, so a
        // non-resuming init serve (`start == 0`) must not re-plan: the second
        // plan is a full media-source resolution (3 DB round trips) plus an arg
        // rebuild, and it produced a byte-identical plan.
        let tmp = tempfile::tempdir().unwrap();
        let script = FakeScript {
            segment_files: vec!["out0.mp4".to_owned()],
            extra_files: vec!["out-1.mp4".to_owned(), "out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        };
        let (mgr, recorded) = manager_with(tmp.path(), script, "mp4");
        let init_req = HlsStreamRequest {
            segment_container: Some("mp4".to_owned()),
            ..req()
        };
        let served = mgr.dynamic_segment(&init_req, -1, false).await.unwrap();
        assert!(served.path.ends_with("out-1.mp4"));
        let calls = recorded.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(false, Some(0))],
            "one plan for a non-resuming init serve"
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

    /// Upstream `HlsSegmentController.GetFileResult` refreshes the owning
    /// transcode's keep-alive on every legacy segment/playlist serve. Ferrofin
    /// carried the comment but not the call: a client driving playback through
    /// the legacy `hls` routes never restamped `last_activity`, so the idle
    /// reaper killed its ffmpeg 60s in — mid-playback — unless the client also
    /// happened to send playback-progress pings.
    #[tokio::test]
    async fn legacy_serve_refreshes_the_owning_jobs_keep_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let script = FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        };
        let (mgr, _) = manager_with(tmp.path(), script, "ts");
        // Start a real (fake-backed) job so the registry has one to keep alive.
        mgr.dynamic_segment(&req(), 0, false).await.unwrap();
        let key = tmp.path().join("out.m3u8").to_string_lossy().into_owned();

        // Let the idle clock run, then serve one of that job's segments the way
        // the legacy route does.
        std::fs::write(tmp.path().join("out7.ts"), b"seg").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let before = mgr
            .manager
            .millis_since_activity(&key, TranscodingJobType::Hls)
            .expect("job registered");
        assert!(before >= 50, "idle clock should have advanced: {before}ms");

        mgr.resolve_transcode_file("out7.ts", false).await.unwrap();

        let after = mgr
            .manager
            .millis_since_activity(&key, TranscodingJobType::Hls)
            .expect("job still registered");
        assert!(
            after < before,
            "legacy serve must restamp the owning job: {before}ms -> {after}ms"
        );
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
        assert_eq!(served.content_type, "application/vnd.apple.mpegurl");
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
        assert_eq!(mime_for_extension(".m3u8"), "application/vnd.apple.mpegurl");
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
