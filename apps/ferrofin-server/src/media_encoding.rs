//! Media-encoding composition — assembles the ffmpeg-backed transcode/HLS pair
//! and the attachment extractor that the composition root injects via
//! [`AppState::with_media_encoding`](ferrofin_api::AppState::with_media_encoding).
//!
//! Port of the Autofac registration of `ITranscodeManager` +
//! `IDynamicHlsPlaylistGenerator` (→ the HLS stream manager) and
//! `IAttachmentExtractor` in `Jellyfin.Server`'s `Startup`. Everything below the
//! [`StreamStatePlanner`](ferrofin_hls::StreamStatePlanner) seam
//! (`TokioSegmentTranscoder` → `TranscodeManagerImpl` → `HlsStreamManagerImpl`)
//! is already ported in `ferrofin-mediaencoding`/`ferrofin-hls`; this unit builds the
//! concrete [`FerrofinStreamStatePlanner`](crate::planner::FerrofinStreamStatePlanner)
//! that feeds it and the two small server-local adapters the attachment extractor
//! needs (a [`MediaSourceResolver`] over the [`MediaSourceManager`] and a real
//! ffmpeg/filesystem [`AttachmentIo`]).

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_core::FerrofinServerApplicationPaths;
use ferrofin_hls::{DynamicHlsPlaylistGenerator, EncodingOptionsProvider, HlsStreamManagerImpl};
use ferrofin_mediaencoding::attachments::{AttachmentIo, MediaSourceResolver};
use ferrofin_mediaencoding::subtitles::{
    SubtitleEditParser, SubtitleEncoder as PureSubtitleEncoder, SubtitleEncoderImpl, SubtitleIo,
    subtitle_extraction_timeout,
};
use ferrofin_mediaencoding::{
    AttachmentExtractorImpl, EncodingHelper, NoopSessionReporter, TokioSegmentTranscoder,
    TranscodeManagerImpl,
};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::Video3DFormat;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::MediaSourceManager;
use ferrofin_traits::media_encoding::SubtitleEncoder;
use ferrofin_traits::media_encoding::{
    AttachmentExtractor, HlsStreamManager, MediaEncoder, MediaInfoRequest,
};
use ferrofin_traits::persistence::KeyframeRepository;
use ferrofin_traits::system::PathManager;
use uuid::Uuid;

use crate::bootstrap::FfmpegPaths;
use crate::encoding_options::EncodingOptionsReader;
use crate::planner::FerrofinStreamStatePlanner;

/// How often the transcode idle reaper sweeps for consumerless jobs.
///
/// A job dies within (its ping timeout + one sweep); 10s adds at most a sixth
/// of the 60s HLS timeout as latency while keeping the scan negligible.
const IDLE_REAPER_SWEEP_SECS: u64 = 10;
/// The managers the media-encoding chain only decorates itself with: absent in
/// the unit tests that build the chain without a database behind it.
pub struct MediaEncodingExtras {
    /// Resolves item/series/library display names for the transcode logs.
    pub library: Option<Arc<dyn ferrofin_traits::library::LibraryManager>>,
    /// The configured libraries, which name the library an item's location
    /// belongs to in those logs.
    pub virtual_folders: Option<Arc<dyn ferrofin_traits::library::VirtualFolderManager>>,
    /// Supplies the item's trickplay tile resolutions for the master playlist.
    pub trickplay: Option<Arc<dyn ferrofin_traits::trickplay::TrickplayManager>>,
    /// Releases the live stream a killed job was reading.
    pub sessions: Option<Arc<dyn ferrofin_traits::session::SessionManager>>,
    /// Stores the keyframes remux playlists are cut on. Without it the
    /// playlist generator has no keyframe extractor, and every playlist uses
    /// equal-length segments.
    pub keyframes: Option<Arc<dyn KeyframeRepository>>,
}

/// The playlist generator's encoding options: the live configuration, as
/// upstream reads `GetEncodingOptions()` on every playlist request.
struct GeneratorEncodingOptions {
    config: Arc<dyn ServerConfigurationManager>,
    reader: EncodingOptionsReader,
}

#[async_trait]
impl EncodingOptionsProvider for GeneratorEncodingOptions {
    async fn get_encoding_options(&self) -> EncodingOptions {
        self.reader.read(self.config.as_ref()).await
    }
}

/// Port of `AddHlsPlaylistGenerator`: the HLS playlist generator over the live
/// encoding options and the cached ffprobe and Matroska keyframe extractors,
/// of which it keeps the metadata-based (Matroska) one. Without a keyframe
/// repository it has no extractor.
fn playlist_generator(
    config: &Arc<dyn ServerConfigurationManager>,
    encoder: &Arc<dyn MediaEncoder>,
    keyframes: Option<Arc<dyn KeyframeRepository>>,
) -> Arc<DynamicHlsPlaylistGenerator<GeneratorEncodingOptions>> {
    let extractors = keyframes
        .map(|repository| {
            ferrofin_core::keyframe_extractors::keyframe_extractors(repository, Arc::clone(encoder))
        })
        .unwrap_or_default();
    Arc::new(DynamicHlsPlaylistGenerator::new(
        GeneratorEncodingOptions {
            config: Arc::clone(config),
            reader: EncodingOptionsReader::new("playlist generator"),
        },
        extractors,
    ))
}

/// The one thing job teardown needs from the session layer: release a live
/// stream no session still wants (`ISessionManager.CloseLiveStreamIfNeededAsync`).
#[async_trait]
trait LiveStreamReleaser: Send + Sync {
    /// Releases `session_id`'s hold on `live_stream_id`, closing the stream
    /// once no session holds it, or at once when no session registered it.
    async fn release(&self, live_stream_id: &str, session_id: &str) -> Result<(), ServiceError>;

    /// Publishes a starting job's details against `device_id`.
    async fn publish_transcoding_info(
        &self,
        device_id: &str,
        info: &ferrofin_model::session::TranscodingInfo,
    ) -> Result<(), ServiceError>;
}

#[async_trait]
impl LiveStreamReleaser for Arc<dyn ferrofin_traits::session::SessionManager> {
    async fn publish_transcoding_info(
        &self,
        device_id: &str,
        info: &ferrofin_model::session::TranscodingInfo,
    ) -> Result<(), ServiceError> {
        self.report_transcoding_info(device_id, info).await
    }

    async fn release(&self, live_stream_id: &str, session_id: &str) -> Result<(), ServiceError> {
        self.close_live_stream_if_needed(live_stream_id, session_id)
            .await
    }
}

/// The [`SessionReporter`] the server runs with: on teardown it releases the
/// live stream the job was reading.
///
/// Port of `TranscodeManager`'s
/// `CloseLiveStreamIfNeededAsync(job.LiveStreamId, job.PlaySessionId)`. Without
/// it a client that disappears without a `PlaybackStopped` — a killed browser
/// tab, a cast receiver that drops off the network — leaves its tuner open
/// until the process exits, and the next tune fails on a busy tuner.
///
/// `report_started` publishes what the job is — codecs, container, size,
/// accelerator — the moment it spawns, which is what puts a transcode on the
/// dashboard at all. The runtime calls it again with framerate, completion and
/// bitrate from changed stderr statistics; `report_progress` remains the
/// separate diagnostics hook and is not required by this adapter.
struct LiveStreamReleasingReporter<R: LiveStreamReleaser> {
    releaser: R,
}

#[async_trait]
impl<R: LiveStreamReleaser> ferrofin_mediaencoding::SessionReporter
    for LiveStreamReleasingReporter<R>
{
    async fn report_progress(
        &self,
        _job: &ferrofin_traits::media_encoding::TranscodingJobHandle,
        _progress: ferrofin_traits::media_encoding::TranscodingProgress,
    ) {
    }

    async fn report_started(
        &self,
        job: &ferrofin_traits::media_encoding::TranscodingJobHandle,
        info: ferrofin_model::session::TranscodingInfo,
    ) {
        let Some(device_id) = job.device_id.as_deref().filter(|d| !d.is_empty()) else {
            return;
        };
        if let Err(error) = self
            .releaser
            .publish_transcoding_info(device_id, &info)
            .await
        {
            // Best effort: a dashboard panel is not worth failing a transcode.
            tracing::warn!(%device_id, %error, "reporting the transcode to the session failed");
        }
    }

    async fn on_job_killed(
        &self,
        job: &ferrofin_traits::media_encoding::TranscodingJobHandle,
        _delete_files: bool,
        close_live_stream: bool,
    ) {
        if !close_live_stream {
            return;
        }
        // `!string.IsNullOrWhiteSpace(job.LiveStreamId)`; the session id upstream
        // passes is the play-session id the job was registered under.
        let Some(live_stream_id) = job
            .live_stream_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let session = job.play_session_id.as_deref().unwrap_or_default();
        if let Err(error) = self.releaser.release(live_stream_id, session).await {
            // Best effort, exactly as upstream's fire-and-forget teardown: a
            // failure here must not stop the rest of the kill path.
            tracing::warn!(%live_stream_id, %error, "releasing the job's live stream failed");
        }
    }
}

/// Starts the pause-key probe in the background, publishing into `slot`.
///
/// The probe is a real-time ffmpeg job, so it must not delay boot; a transcode
/// that starts before it finishes picks the keys up on a later throttle tick.
/// Nothing runs for the placeholder a failed discovery leaves (its capabilities
/// are not [`validated`](ferrofin_mediaencoding::FfmpegCapabilities::validated)):
/// that is a binary the server just declared unavailable.
fn spawn_pause_key_probe(
    handle: &tokio::runtime::Handle,
    ffmpeg: &FfmpegPaths,
    slot: ferrofin_mediaencoding::transcoding::throttler::PauseKeySlot,
) {
    if !ffmpeg.capabilities.validated() {
        return;
    }
    handle.spawn(tracing::Instrument::instrument(
        crate::bootstrap::probe_pause_keys(
            ffmpeg.ffmpeg.clone(),
            ffmpeg.capabilities.ffmpeg_version(),
            slot,
        ),
        tracing::info_span!(
            "ffmpeg_pause_probe",
            task = "ffmpeg_pause_probe",
            trigger = "startup"
        ),
    ));
}

/// Builds the concrete media-encoding pair injected via
/// [`AppState::with_media_encoding`](ferrofin_api::AppState::with_media_encoding).
///
/// Returns `(hls, attachments)`:
/// - `hls` — the [`HlsStreamManagerImpl`] wiring the
///   [`FerrofinStreamStatePlanner`](crate::planner::FerrofinStreamStatePlanner) →
///   [`TokioSegmentTranscoder`] → [`TranscodeManagerImpl`] →
///   [`DynamicHlsPlaylistGenerator`] chain (the real transcode runtime, not the
///   `DisabledHlsStreamManager` stub);
/// - `attachments` — the [`AttachmentExtractorImpl`] over the real ffmpeg/
///   filesystem [`AttachmentIo`] and a [`MediaSourceResolver`] adapting the
///   [`MediaSourceManager`] (not the `DisabledAttachmentExtractor` stub).
///
/// The `NoopSessionReporter` stands in only when no session manager was
/// injected. With one, [`LiveStreamReleasingReporter`] publishes a starting
/// job to the session layer and releases its live stream on teardown;
/// killed-job partial-file cleanup is the manager's `FsFileCleaner`.
///
/// `ffmpeg` carries the startup capability probes, which the planner reads for
/// every encoding decision: the `-filters` list gates the jellyfin-ffmpeg-only
/// `tonemapx` software tonemap (the planner falls back to the vanilla zscale
/// chain without it), the `-encoders` list lets the audio path prefer
/// `aac_at`/`libfdk_aac` over native `aac`, and the hardware lists and version
/// gates drive the whole hardware-acceleration matrix.
#[must_use]
pub fn build_media_encoding(
    media_sources: Arc<dyn MediaSourceManager>,
    encoder: Arc<dyn MediaEncoder>,
    config: Arc<dyn ServerConfigurationManager>,
    paths: Arc<FerrofinServerApplicationPaths>,
    path_manager: Arc<dyn PathManager>,
    ffmpeg: &FfmpegPaths,
    extras: MediaEncodingExtras,
) -> (
    Arc<dyn HlsStreamManager>,
    Arc<dyn AttachmentExtractor>,
    Arc<dyn SubtitleEncoder>,
) {
    let library = extras.library;
    let generator = playlist_generator(&config, &encoder, extras.keyframes);
    // ---- subtitle encoder (real ffmpeg extraction + charset/format conv) ---
    // Built first: the planner also consumes it, resolving a burned text
    // subtitle to its cached extracted file so the `subtitles` filter doesn't
    // re-demux the whole source on every ffmpeg start.
    let sub_resolver = Arc::new(MediaSourceManagerResolver {
        media_sources: Arc::clone(&media_sources),
    });
    let sub_io = FfmpegSubtitleIo {
        path_manager: Arc::clone(&path_manager),
        ffmpeg_path: encoder.encoder_path(),
        config: Arc::clone(&config),
        encoding_reader: EncodingOptionsReader::new("subtitle extraction"),
    };
    let pure_encoder = Arc::new(PureSubtitleEncoder::new(SubtitleEditParser::new(), sub_io));
    let subtitles: Arc<dyn SubtitleEncoder> =
        Arc::new(SubtitleEncoderImpl::new(pure_encoder, sub_resolver));

    // ---- attachment extractor (real ffmpeg + filesystem) ------------------
    // Built before the planner, which extracts a file's fonts before a
    // subtitle burn-in that may need them (`TranscodeManager.StartFfMpeg`).
    let resolver = Arc::new(MediaSourceManagerResolver {
        media_sources: Arc::clone(&media_sources),
    });
    let io = Arc::new(FfmpegAttachmentIo {
        path_manager: Arc::clone(&path_manager),
    });
    // `AttachmentExtractorImpl<E, …>` holds an `Arc<E>` with `E: Sized`, so the
    // trait-object encoder is wrapped in the sized [`DynMediaEncoder`] newtype.
    let attachments: Arc<dyn AttachmentExtractor> = Arc::new(AttachmentExtractorImpl::new(
        Arc::new(DynMediaEncoder(Arc::clone(&encoder))),
        resolver,
        io,
    ));

    // ---- HLS transcode chain (below the planner seam) ---------------------
    let ffmpeg_path = encoder.encoder_path();
    let planner = FerrofinStreamStatePlanner::new(
        media_sources,
        encoder,
        EncodingHelper::new(ffmpeg.capabilities.clone()),
        Arc::clone(&config),
        Arc::clone(&paths),
        Arc::clone(&subtitles),
        ffmpeg.supports_filter("tonemapx"),
    )
    .with_attachments(Arc::clone(&attachments), path_manager);
    let planner = match library {
        Some(library) => planner.with_library(library),
        None => planner,
    };
    let planner = match extras.virtual_folders.clone() {
        Some(folders) => planner.with_virtual_folders(folders),
        None => planner,
    };
    let transcoder = TokioSegmentTranscoder::new();
    // A live transcode holds a tuner: the reporter releases it when the job dies.
    let reporter: Arc<dyn ferrofin_mediaencoding::SessionReporter> = match extras.sessions {
        Some(sessions) => Arc::new(LiveStreamReleasingReporter { releaser: sessions }),
        None => Arc::new(NoopSessionReporter),
    };
    // `TranscodeManager`'s constructor clears the transcode cache before any
    // job can start (`DeleteEncodedMediaCache`, `TranscodeManager.cs:94`).
    let cleared = ferrofin_core::scheduled_tasks::maintenance::delete_encoded_media_cache(&*paths);
    if cleared > 0 {
        tracing::info!(deleted = cleared, "cleared the transcode cache");
    }
    let manager = Arc::new(TranscodeManagerImpl::new(reporter).with_encoding_configuration(config));
    // The idle reaper kills any transcode whose consumers vanished without a
    // stop report (a disconnected cast receiver, a killed browser tab): no
    // active segment request and no session ping within the job's ping timeout
    // → the job dies and its partial files are cleaned up. Spawned only when a
    // runtime exists so the sync composition unit test can still call this.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        spawn_pause_key_probe(&handle, ffmpeg, manager.pause_key_slot());
        handle.spawn(TranscodeManagerImpl::run_transcode_controls(
            Arc::downgrade(&manager),
        ));
        handle.spawn(
            Arc::clone(&manager)
                .run_idle_reaper(std::time::Duration::from_secs(IDLE_REAPER_SWEEP_SECS)),
        );
    }
    let mut hls_impl = HlsStreamManagerImpl::new(
        planner,
        transcoder,
        manager,
        generator,
        paths as Arc<dyn ferrofin_traits::system::ServerApplicationPaths>,
        ffmpeg_path,
    );
    // Without this the master playlist lists no `#EXT-X-IMAGE-STREAM-INF`, so a
    // client sees no trickplay tiles however many the library holds.
    if let Some(trickplay) = extras.trickplay {
        hls_impl = hls_impl.with_trickplay(trickplay);
    }
    let hls: Arc<dyn HlsStreamManager> = Arc::new(hls_impl);

    (hls, attachments, subtitles)
}

/// The real ffmpeg + filesystem [`SubtitleIo`] for the subtitle encoder.
///
/// Mirrors [`FfmpegAttachmentIo`]: cache paths come from the [`PathManager`],
/// file/HTTP reads use `tokio::fs`/`reqwest`, and extraction runs the injected
/// ffmpeg binary through `tokio::process`, bounded by the persisted
/// `SubtitleExtractionTimeoutMinutes` read from `config` on every run.
struct FfmpegSubtitleIo {
    path_manager: Arc<dyn PathManager>,
    ffmpeg_path: String,
    config: Arc<dyn ServerConfigurationManager>,
    /// Reads `encoding.json` on each extraction and reports a broken file once.
    encoding_reader: EncodingOptionsReader,
}

#[async_trait]
impl SubtitleIo for FfmpegSubtitleIo {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        tokio::fs::read(path)
            .await
            .map_err(|e| format!("read {path}: {e}"))
    }

    async fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = reqwest::get(url)
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url}: HTTP {}", resp.status()));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("GET {url} body: {e}"))
    }

    fn path_protocol(&self, path: &str) -> MediaProtocol {
        let lower = path.trim_start().to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            MediaProtocol::Http
        } else {
            MediaProtocol::File
        }
    }

    fn subtitle_cache_path(
        &self,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_extension: &str,
    ) -> Option<String> {
        self.path_manager
            .subtitle_path(media_source_id, subtitle_stream_index, output_extension)
    }

    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<(), String> {
        // ffmpeg won't create missing output directories; the per-source subtitle
        // cache folder may not exist yet on first extraction, so ensure each
        // output's parent exists (C# creates it before spawning ffmpeg).
        for path in output_paths {
            if let Some(parent) = std::path::Path::new(path).parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
        }
        // `RunSubtitleExtractionProcess` reads the timeout on every run.
        let minutes = self
            .encoding_reader
            .read(self.config.as_ref())
            .await
            .subtitle_extraction_timeout_minutes;
        let Some(timeout) = subtitle_extraction_timeout(minutes) else {
            return Err(format!(
                "invalid SubtitleExtractionTimeoutMinutes {minutes}: negative or longer than \
                 the longest supported wait"
            ));
        };
        let parts = ferrofin_mediaencoding::encoder::split_args(args)?;
        let child = tokio::process::Command::new(&self.ffmpeg_path)
            .args(&parts)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn ffmpeg {}: {e}", self.ffmpeg_path))?;
        // From here until ffmpeg exits on its own, any way out — the timeout
        // below, or the request future being dropped (a client disconnecting
        // mid-seek) — kills ffmpeg and deletes its outputs.
        let mut running = RunningExtraction {
            child,
            output_paths,
            finished: false,
        };
        let Ok(waited) = tokio::time::timeout(timeout, running.child.wait()).await else {
            tracing::warn!(
                timeout_minutes = minutes,
                "ffmpeg subtitle extraction timed out; killing it and deleting its outputs"
            );
            return Err(format!(
                "ffmpeg subtitle extraction timed out after {minutes} minute(s)"
            ));
        };
        let status = waited.map_err(|e| format!("wait for ffmpeg {}: {e}", self.ffmpeg_path))?;
        running.finished = true;
        if !status.success() {
            return Err(format!(
                "ffmpeg subtitle extraction exited with {}",
                status.code().unwrap_or(-1)
            ));
        }
        // The extraction is only useful if every requested output was written.
        for path in output_paths {
            if !std::path::Path::new(path).is_file() {
                return Err(format!("ffmpeg did not produce {path}"));
            }
        }
        Ok(())
    }
}

/// A subtitle or attachment extraction ffmpeg that has not exited on its own
/// yet.
///
/// Port of upstream's cancellation path: the request's cancellation token (and,
/// for a subtitle, the timeout linked to it) kills the process (`Kill(true)`,
/// exit code `-1`) and deletes every output it was writing
/// (`ExtractSubtitlesForFile`, `AttachmentExtractor`'s failure path). Here both
/// cases are a drop before `finished` is set — the timeout returns early, and a
/// cancelled request drops the future awaiting it. Without the deletion, a file
/// cut off mid-write would stay in the cache and be served on every later
/// request, because extraction skips any output that already exists.
struct RunningExtraction<'a> {
    child: tokio::process::Child,
    output_paths: &'a [String],
    finished: bool,
}

impl Drop for RunningExtraction<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Kill first, so ffmpeg can't keep writing once its outputs are gone.
        if let Err(error) = self.child.start_kill() {
            tracing::debug!(%error, "the interrupted ffmpeg extraction had already exited");
        }
        for path in self.output_paths {
            match std::fs::remove_file(path) {
                Ok(()) => tracing::debug!(path, "deleted an interrupted extraction output"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::error!(%error, path, "deleting an interrupted ffmpeg extraction's output failed");
                }
            }
        }
    }
}

/// A `Sized` newtype wrapping an `Arc<dyn MediaEncoder>`, delegating every
/// method to the inner trait object.
///
/// [`AttachmentExtractorImpl`] is generic over a `Sized` `E: MediaEncoder`
/// (it stores an `Arc<E>`), so the composition root — which only has the
/// erased `Arc<dyn MediaEncoder>` — wraps it here to satisfy the bound without
/// re-plumbing the extractor to take a trait object.
struct DynMediaEncoder(Arc<dyn MediaEncoder>);

#[async_trait]
impl MediaEncoder for DynMediaEncoder {
    fn encoder_path(&self) -> String {
        self.0.encoder_path()
    }
    fn probe_path(&self) -> String {
        self.0.probe_path()
    }
    async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
        self.0.set_ffmpeg_path().await
    }
    async fn get_media_info(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        self.0.get_media_info(request).await
    }
    async fn extract_audio_image(
        &self,
        path: &str,
        image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        self.0.extract_audio_image(path, image_stream_index).await
    }
    async fn extract_video_image(
        &self,
        input_file: &str,
        container: &str,
        media_source: &MediaSourceInfo,
        video_stream: &MediaStream,
        threed_format: Option<Video3DFormat>,
        offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        self.0
            .extract_video_image(
                input_file,
                container,
                media_source,
                video_stream,
                threed_format,
                offset_ticks,
            )
            .await
    }
    fn get_input_argument(&self, input_file: &str, media_source: &MediaSourceInfo) -> String {
        self.0.get_input_argument(input_file, media_source)
    }
    fn get_time_parameter(&self, ticks: i64) -> String {
        self.0.get_time_parameter(ticks)
    }
    async fn convert_image(&self, input_path: &str, output_path: &str) -> Result<(), ServiceError> {
        self.0.convert_image(input_path, output_path).await
    }
}

/// Adapts the [`MediaSourceManager`] to the attachment extractor's
/// [`MediaSourceResolver`] seam.
///
/// Port of the `IMediaSourceManager.GetPlaybackMediaSources` call inside
/// `GetAttachment`: resolves `(item_id, media_source_id)` to a
/// [`MediaSourceInfo`]; each static source already carries its own attachment
/// rows (so a merged alternate version resolves to ITS attachments).
struct MediaSourceManagerResolver {
    media_sources: Arc<dyn MediaSourceManager>,
}

#[async_trait]
impl MediaSourceResolver for MediaSourceManagerResolver {
    async fn resolve(
        &self,
        item_id: Uuid,
        media_source_id: &str,
    ) -> Result<Option<MediaSourceInfo>, ServiceError> {
        let sources = self
            .media_sources
            .get_static_media_sources(item_id, false, None)
            .await?;
        Ok(sources.into_iter().find(|s| s.id_matches(media_source_id)))
    }
}

/// The real ffmpeg + filesystem [`AttachmentIo`] for the attachment extractor.
///
/// Port of the `Process`/`File`/`Directory` calls in the C# extractor: the cache
/// folder/file paths come from the injected [`PathManager`]
/// (`IPathManager.GetAttachmentFolderPath`/`GetAttachmentPath`), the file checks/
/// reads use `tokio::fs`, and `-dump_attachment` runs through `tokio::process`.
struct FfmpegAttachmentIo {
    path_manager: Arc<dyn PathManager>,
}

#[async_trait]
impl AttachmentIo for FfmpegAttachmentIo {
    fn attachment_folder_path(&self, media_source_id: &str) -> Option<String> {
        self.path_manager.attachment_folder_path(media_source_id)
    }

    fn attachment_path(&self, media_source_id: &str, file_name: &str) -> Option<String> {
        // The path manager keeps a name's leaf and refuses a name with none
        // (`..`, a trailing separator); the extractor then falls back to the
        // attachment's index, as upstream does.
        self.path_manager
            .attachment_path(media_source_id, file_name)
    }

    fn file_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    async fn remove_file(&self, path: &str) -> Result<(), String> {
        match tokio::fs::remove_file(path).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("delete {path}: {error}"))
            }
            _ => Ok(()),
        }
    }

    async fn create_directory(&self, path: &str) -> Result<(), String> {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| format!("create {path}: {e}"))
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        tokio::fs::read(path)
            .await
            .map_err(|e| format!("read {path}: {e}"))
    }

    async fn run_ffmpeg(
        &self,
        ffmpeg_path: &str,
        args: &str,
        working_dir: Option<&str>,
        outputs: &[String],
    ) -> Result<i32, String> {
        // `args` is a pre-built ffmpeg command line (a single space-joined
        // string, matching the C# `arguments` string), split with the same
        // splitter every other string-built ffmpeg run uses.
        let parts = ferrofin_mediaencoding::encoder::split_args(args)?;
        let mut command = tokio::process::Command::new(ffmpeg_path);
        if let Some(dir) = working_dir {
            command.current_dir(dir);
        }
        let child = command
            .args(&parts)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn ffmpeg {ffmpeg_path}: {e}"))?;
        // Until ffmpeg exits on its own, a cancelled request (its future
        // dropped) kills it and deletes `outputs`. Upstream has no timeout
        // here, only the cancellation (owner decision 2026-10-25).
        let mut running = RunningExtraction {
            child,
            output_paths: outputs,
            finished: false,
        };
        let status = running
            .child
            .wait()
            .await
            .map_err(|e| format!("wait for ffmpeg {ffmpeg_path}: {e}"))?;
        running.finished = true;
        Ok(status.code().unwrap_or(-1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_model::entities_media::MediaAttachment;
    use ferrofin_model::media_info::LiveStreamRequest;

    /// A fake [`MediaSourceManager`] returning fixed sources + attachments.
    struct FakeSources {
        sources: Vec<MediaSourceInfo>,
        attachments: Vec<MediaAttachment>,
    }

    #[async_trait]
    impl MediaSourceManager for FakeSources {
        async fn get_media_streams(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaStream>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_media_attachments(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaAttachment>, ServiceError> {
            Ok(self.attachments.clone())
        }
        async fn get_playback_media_sources(
            &self,
            _item_id: Uuid,
            _user_id: Uuid,
            _allow_media_probe: bool,
            _enable_path_substitution: bool,
        ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
            Ok(self.sources.clone())
        }
        async fn get_static_media_sources(
            &self,
            _item_id: Uuid,
            _enable_path_substitution: bool,
            _user_id: Option<Uuid>,
        ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
            Ok(self.sources.clone())
        }
        async fn open_live_stream(
            &self,
            _request: &LiveStreamRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Err(ServiceError::backend("no"))
        }
        async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
            Err(ServiceError::backend("no"))
        }
        async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A fake [`MediaEncoder`] recording the delegated calls it received.
    struct RecordingEncoder;

    #[async_trait]
    impl MediaEncoder for RecordingEncoder {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "ffprobe".to_owned()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            _request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Ok(MediaSourceInfo::default())
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            Ok("audio.jpg".to_owned())
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            Ok("video.jpg".to_owned())
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            format!("file:{input_file}")
        }
        fn get_time_parameter(&self, ticks: i64) -> String {
            format!("-ss {ticks}")
        }
        async fn convert_image(
            &self,
            _input_path: &str,
            _output_path: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A fake [`PathManager`] with a fixed attachment folder root.
    struct FakePathManager {
        root: String,
    }

    impl PathManager for FakePathManager {
        fn trickplay_directory(
            &self,
            _item_id: Uuid,
            _media: ferrofin_traits::system::MediaLocation<'_>,
            _save_with_media: bool,
        ) -> String {
            String::new()
        }
        fn subtitle_path(
            &self,
            _media_source_id: &str,
            _stream_index: i32,
            _extension: &str,
        ) -> Option<String> {
            None
        }
        fn subtitle_folder_path(&self, _media_source_id: &str) -> Option<String> {
            None
        }
        fn attachment_path(&self, media_source_id: &str, file_name: &str) -> Option<String> {
            Some(format!("{}/{media_source_id}/{file_name}", self.root))
        }
        fn attachment_folder_path(&self, media_source_id: &str) -> Option<String> {
            Some(format!("{}/{media_source_id}", self.root))
        }
        fn chapter_image_folder_path(&self, _item_id: Uuid, _media_path: &str) -> String {
            String::new()
        }
        fn chapter_image_path(
            &self,
            _item_id: Uuid,
            _media_path: &str,
            _chapter_position_ticks: i64,
            _date_modified_ticks: i64,
        ) -> String {
            String::new()
        }
        fn extracted_data_paths(
            &self,
            _item_id: Uuid,
            _media_path: ferrofin_traits::system::MediaLocation<'_>,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    fn config_and_paths() -> (
        Arc<dyn ServerConfigurationManager>,
        Arc<FerrofinServerApplicationPaths>,
    ) {
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            "/data",
            std::path::PathBuf::from("/data/log"),
            "/config",
            "/cache",
            "/web",
        ));
        let config: Arc<dyn ServerConfigurationManager> =
            Arc::new(FakeConfig(Arc::clone(&paths), EncodingOptions::default()));
        (config, paths)
    }

    /// [`config_and_paths`] rooted at `root`. The composition clears the
    /// transcode directory, so a test that builds it must own its paths.
    fn config_and_paths_in(
        root: &std::path::Path,
    ) -> (
        Arc<dyn ServerConfigurationManager>,
        Arc<FerrofinServerApplicationPaths>,
    ) {
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            root.join("data"),
            root.join("log"),
            root.join("config"),
            root.join("cache"),
            root.join("web"),
        ));
        let config: Arc<dyn ServerConfigurationManager> =
            Arc::new(FakeConfig(Arc::clone(&paths), EncodingOptions::default()));
        (config, paths)
    }

    /// A fake [`ServerConfigurationManager`] exposing the paths and fixed
    /// encoding options.
    struct FakeConfig(Arc<FerrofinServerApplicationPaths>, EncodingOptions);

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
            Ok(self.1.clone())
        }
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            Arc::clone(&self.0) as Arc<_>
        }
        async fn configuration(
            &self,
        ) -> Result<std::sync::Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError>
        {
            Ok(std::sync::Arc::new(
                ferrofin_model::configuration::ServerConfiguration::default(),
            ))
        }
        async fn update_configuration(
            &self,
            _configuration: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn source(id: &str) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some(id.to_owned()),
            path: Some("/media/x.mkv".to_owned()),
            container: Some("mkv".to_owned()),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Video,
                codec: Some("h264".to_owned()),
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        }
    }

    #[test]
    fn build_media_encoding_produces_real_pair() {
        let media_sources: Arc<dyn MediaSourceManager> = Arc::new(FakeSources {
            sources: vec![source("abc")],
            attachments: Vec::new(),
        });
        let encoder: Arc<dyn MediaEncoder> = Arc::new(RecordingEncoder);
        let tmp = tempfile::tempdir().unwrap();
        let (config, paths) = config_and_paths_in(tmp.path());
        let path_manager: Arc<dyn PathManager> = Arc::new(FakePathManager {
            root: tmp.path().join("att").to_string_lossy().into_owned(),
        });
        // The trio builds without panicking; every slot is a real trait object.
        let ffmpeg = FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
            chromaprint_muxer: false,
        };
        let (_hls, _attachments, _subtitles) = build_media_encoding(
            media_sources,
            encoder,
            config,
            paths,
            path_manager,
            &ffmpeg,
            MediaEncodingExtras {
                library: None,
                virtual_folders: None,
                trickplay: None,
                sessions: None,
                keyframes: None,
            },
        );
    }

    /// The composition runs upstream's `DeleteEncodedMediaCache`
    /// (`TranscodeManager.cs:94,718`): every file a crash left under the
    /// transcode directory is gone before any job starts, whatever its age;
    /// the directories stay.
    #[test]
    fn build_media_encoding_clears_the_transcode_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, paths) = config_and_paths_in(tmp.path());
        let job = tmp.path().join("cache/transcodes/0123abcd");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(job.join("0123abcd0.mp4"), b"segment").unwrap();
        std::fs::write(
            tmp.path().join("cache/transcodes/0123abcd.m3u8"),
            b"#EXTM3U",
        )
        .unwrap();
        let ffmpeg = FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
            chromaprint_muxer: false,
        };
        let _ = build_media_encoding(
            Arc::new(FakeSources {
                sources: Vec::new(),
                attachments: Vec::new(),
            }),
            Arc::new(RecordingEncoder),
            config,
            paths,
            Arc::new(FakePathManager {
                root: tmp.path().join("att").to_string_lossy().into_owned(),
            }),
            &ffmpeg,
            MediaEncodingExtras {
                library: None,
                virtual_folders: None,
                trickplay: None,
                sessions: None,
                keyframes: None,
            },
        );
        assert!(!job.join("0123abcd0.mp4").exists());
        assert!(!tmp.path().join("cache/transcodes/0123abcd.m3u8").exists());
        assert!(job.is_dir(), "directories stay");
    }

    /// Records what job teardown asked the session layer to release.
    #[derive(Default)]
    struct RecordingReleaser {
        released: std::sync::Mutex<Vec<(String, String)>>,
        /// Every transcode reported to the session layer, as
        /// `(device_id, hardware_acceleration_type)`.
        reported:
            std::sync::Mutex<Vec<(String, ferrofin_model::entities::HardwareAccelerationType)>>,
        fail: bool,
    }

    #[async_trait]
    impl LiveStreamReleaser for RecordingReleaser {
        async fn publish_transcoding_info(
            &self,
            device_id: &str,
            info: &ferrofin_model::session::TranscodingInfo,
        ) -> Result<(), ServiceError> {
            self.reported.lock().expect("lock").push((
                device_id.to_owned(),
                info.hardware_acceleration_type
                    .unwrap_or(ferrofin_model::entities::HardwareAccelerationType::none),
            ));
            Ok(())
        }

        async fn release(
            &self,
            live_stream_id: &str,
            session_id: &str,
        ) -> Result<(), ServiceError> {
            self.released
                .lock()
                .expect("lock")
                .push((live_stream_id.to_owned(), session_id.to_owned()));
            if self.fail {
                return Err(ServiceError::backend("closing failed"));
            }
            Ok(())
        }
    }

    fn killed_job(
        live_stream_id: Option<&str>,
        play_session_id: Option<&str>,
    ) -> ferrofin_traits::media_encoding::TranscodingJobHandle {
        ferrofin_traits::media_encoding::TranscodingJobHandle {
            play_session_id: play_session_id.map(str::to_owned),
            path: "/cache/out.m3u8".to_owned(),
            job_type: ferrofin_traits::media_encoding::TranscodingJobType::Hls,
            device_id: Some("dev".to_owned()),
            live_stream_id: live_stream_id.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn killing_a_live_job_releases_its_stream() {
        use ferrofin_mediaencoding::SessionReporter as _;
        // The whole point: the idle reaper kills a job whose client vanished
        // without a Stop report, and that must hand the tuner back.
        let reporter = LiveStreamReleasingReporter {
            releaser: RecordingReleaser::default(),
        };
        reporter
            .on_job_killed(&killed_job(Some("live-1"), Some("sess-1")), true, true)
            .await;
        assert_eq!(
            *reporter.releaser.released.lock().expect("lock"),
            vec![("live-1".to_owned(), "sess-1".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_job_with_no_live_stream_releases_nothing() {
        use ferrofin_mediaencoding::SessionReporter as _;
        // `!string.IsNullOrWhiteSpace(job.LiveStreamId)` — an ordinary file
        // transcode, and a blank id, both skip the call entirely.
        let reporter = LiveStreamReleasingReporter {
            releaser: RecordingReleaser::default(),
        };
        for job in [
            killed_job(None, Some("sess-1")),
            killed_job(Some("   "), Some("sess-1")),
        ] {
            reporter.on_job_killed(&job, true, true).await;
        }
        assert!(reporter.releaser.released.lock().expect("lock").is_empty());
        // A job with no play session still releases: upstream passes the empty
        // string through to `CloseLiveStreamIfNeededAsync`.
        reporter
            .on_job_killed(&killed_job(Some("live-2"), None), false, true)
            .await;
        assert_eq!(
            reporter.releaser.released.lock().expect("lock")[0],
            ("live-2".to_owned(), String::new())
        );
    }

    #[tokio::test]
    async fn a_seek_restart_or_explicit_stop_keeps_the_live_stream() {
        use ferrofin_mediaencoding::SessionReporter as _;
        // Upstream's `closeLiveStream: false` callers: the seek restart is about
        // to re-open ffmpeg on this very stream, and an explicit stop releases
        // through the session layer instead. Releasing here would tear the tuner
        // out from under the restart.
        let reporter = LiveStreamReleasingReporter {
            releaser: RecordingReleaser::default(),
        };
        reporter
            .on_job_killed(&killed_job(Some("live-4"), Some("sess")), false, false)
            .await;
        assert!(reporter.releaser.released.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn a_failed_release_does_not_escape_teardown() {
        use ferrofin_mediaencoding::SessionReporter as _;
        let reporter = LiveStreamReleasingReporter {
            releaser: RecordingReleaser {
                fail: true,
                ..RecordingReleaser::default()
            },
        };
        // Returns normally (the kill path must finish); the attempt was made.
        reporter
            .on_job_killed(&killed_job(Some("live-3"), Some("sess")), true, true)
            .await;
        assert_eq!(reporter.releaser.released.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn a_starting_transcode_tells_the_session_what_hardware_it_uses() {
        use ferrofin_mediaencoding::SessionReporter as _;
        use ferrofin_model::entities::HardwareAccelerationType;
        let reporter = LiveStreamReleasingReporter {
            releaser: RecordingReleaser::default(),
        };
        let info = ferrofin_model::session::TranscodingInfo {
            hardware_acceleration_type: Some(HardwareAccelerationType::nvenc),
            ..ferrofin_model::session::TranscodingInfo::default()
        };
        reporter
            .report_started(&killed_job(None, Some("sess")), info)
            .await;
        assert_eq!(
            *reporter.releaser.reported.lock().expect("lock"),
            vec![("dev".to_owned(), HardwareAccelerationType::nvenc)]
        );
    }

    #[tokio::test]
    async fn a_job_with_no_device_reports_nothing() {
        // There is nothing to attach the panel to.
        use ferrofin_mediaencoding::SessionReporter as _;
        let reporter = LiveStreamReleasingReporter {
            releaser: RecordingReleaser::default(),
        };
        let mut job = killed_job(None, Some("sess"));
        job.device_id = None;
        reporter
            .report_started(&job, ferrofin_model::session::TranscodingInfo::default())
            .await;
        assert!(reporter.releaser.reported.lock().expect("lock").is_empty());
    }

    /// A [`TrickplayManager`](ferrofin_traits::trickplay::TrickplayManager) with
    /// one canned resolution for every item, so a master playlist that reached
    /// it says so. Every other method is unused by this test.
    struct FakeTrickplay;

    #[async_trait]
    impl ferrofin_traits::trickplay::TrickplayManager for FakeTrickplay {
        async fn get_trickplay_resolutions(
            &self,
            _item_id: uuid::Uuid,
        ) -> Result<
            std::collections::HashMap<i32, ferrofin_db::entities::playback::TrickplayInfoEntity>,
            ServiceError,
        > {
            Ok(std::collections::HashMap::from([(
                320,
                ferrofin_db::entities::playback::TrickplayInfoEntity {
                    item_id: String::new(),
                    width: 320,
                    height: 180,
                    tile_width: 10,
                    tile_height: 10,
                    thumbnail_count: 100,
                    interval: 10_000,
                    bandwidth: 12_345,
                },
            )]))
        }
        async fn refresh_trickplay_data(
            &self,
            _item_id: uuid::Uuid,
            _replace: bool,
            _library_options: &ferrofin_model::configuration::LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_trickplay_items(
            &self,
            _limit: i32,
            _offset: i32,
        ) -> Result<Vec<ferrofin_db::entities::playback::TrickplayInfoEntity>, ServiceError>
        {
            unimplemented!("fake")
        }
        async fn save_trickplay_info(
            &self,
            _info: &ferrofin_db::entities::playback::TrickplayInfoEntity,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn delete_trickplay_data(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
            unimplemented!("fake")
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
            unimplemented!("fake")
        }
        async fn get_hls_playlist(
            &self,
            _item_id: uuid::Uuid,
            _width: i32,
            _api_key: Option<&str>,
        ) -> Result<Option<String>, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_trickplay_tile_path(
            &self,
            _item_id: uuid::Uuid,
            _width: i32,
            _index: i32,
        ) -> Result<Option<String>, ServiceError> {
            unimplemented!("fake")
        }
    }

    #[tokio::test]
    async fn the_built_chain_carries_the_extras_it_was_given() {
        // The trickplay bug this guards against was exactly "collaborator built,
        // unit-tested, never attached": the master playlist listed no
        // `#EXT-X-IMAGE-STREAM-INF` in production while every unit test passed.
        // Assert through the BUILT chain, not the builder.
        // A finite source with a guid id: a live stream lists no trickplay
        // upstream either, and the tile lookup parses the media-source id.
        let source_id = uuid::Uuid::from_u128(0xABC).simple().to_string();
        let mut vod = source(&source_id);
        vod.run_time_ticks = Some(90 * 60 * 10_000_000);
        let media_sources: Arc<dyn MediaSourceManager> = Arc::new(FakeSources {
            sources: vec![vod],
            attachments: Vec::new(),
        });
        let encoder: Arc<dyn MediaEncoder> = Arc::new(RecordingEncoder);
        let directory = tempfile::tempdir().expect("owned composition paths");
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            directory.path().join("data"),
            directory.path().join("log"),
            directory.path().join("config"),
            directory.path().join("cache"),
            directory.path().join("web"),
        ));
        let config: Arc<dyn ServerConfigurationManager> =
            Arc::new(FakeConfig(Arc::clone(&paths), EncodingOptions::default()));
        let path_manager: Arc<dyn PathManager> = Arc::new(FakePathManager {
            root: "/cache/att".to_owned(),
        });
        let ffmpeg = FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
            chromaprint_muxer: false,
        };
        let (hls, _attachments, _subtitles) = build_media_encoding(
            Arc::clone(&media_sources),
            encoder,
            config,
            paths,
            path_manager,
            &ffmpeg,
            MediaEncodingExtras {
                library: None,
                virtual_folders: None,
                trickplay: Some(Arc::new(FakeTrickplay)),
                sessions: None,
                keyframes: None,
            },
        );

        let request = ferrofin_traits::media_encoding::HlsStreamRequest {
            item_id: uuid::Uuid::from_u128(1),
            media_source_id: Some(source_id.clone()),
            play_session_id: Some("sess".to_owned()),
            device_id: Some("dev".to_owned()),
            segment_container: Some("ts".to_owned()),
            query_string: "?x=1".to_owned(),
            enable_trickplay: true,
            ..ferrofin_traits::media_encoding::HlsStreamRequest::default()
        };
        let playlist = hls
            .master_playlist(&request, false)
            .await
            .expect("master playlist");
        assert!(
            playlist.contains("#EXT-X-IMAGE-STREAM-INF"),
            "the trickplay manager the chain was given must reach the playlist: {playlist}"
        );
    }

    /// A keyframe repository holding rows in memory.
    #[derive(Default)]
    struct StoredKeyframes(
        std::sync::Mutex<
            std::collections::HashMap<Uuid, ferrofin_db::entities::base_items::KeyframeDataEntity>,
        >,
    );

    #[async_trait]
    impl KeyframeRepository for StoredKeyframes {
        async fn get_keyframe_data(
            &self,
            item_id: Uuid,
        ) -> Result<Vec<ferrofin_db::entities::base_items::KeyframeDataEntity>, ServiceError>
        {
            Ok(self
                .0
                .lock()
                .expect("rows")
                .get(&item_id)
                .cloned()
                .into_iter()
                .collect())
        }
        async fn save_keyframe_data(
            &self,
            item_id: Uuid,
            data: &ferrofin_db::entities::base_items::KeyframeDataEntity,
        ) -> Result<(), ServiceError> {
            self.0.lock().expect("rows").insert(item_id, data.clone());
            Ok(())
        }
        async fn delete_keyframe_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
            self.0.lock().expect("rows").remove(&item_id);
            Ok(())
        }
    }

    /// The `#EXTINF` lengths of a playlist.
    fn segment_lengths(playlist: &str) -> Vec<&str> {
        playlist
            .lines()
            .filter_map(|line| line.strip_prefix("#EXTINF:"))
            .filter_map(|rest| rest.strip_suffix(", nodesc"))
            .collect()
    }

    /// The playlist for a 12 s `file` whose stored keyframes are at 0, 4.2,
    /// 6.5 and 11 s, cut into 6 s segments.
    async fn playlist_for(file: &str, remux: bool, options: EncodingOptions) -> String {
        let item = Uuid::from_u128(0x4B);
        let repository = Arc::new(StoredKeyframes::default());
        repository
            .save_keyframe_data(
                item,
                &ferrofin_db::entities::base_items::KeyframeDataEntity {
                    item_id: ferrofin_db::store::guid_to_db(item),
                    keyframe_ticks: Some("[0,42000000,65000000,110000000]".to_owned()),
                    total_duration: 120_000_000,
                },
            )
            .await
            .expect("seed");
        let (_, paths) = config_and_paths();
        let config: Arc<dyn ServerConfigurationManager> = Arc::new(FakeConfig(paths, options));
        let encoder: Arc<dyn MediaEncoder> = Arc::new(RecordingEncoder);
        let generator = playlist_generator(
            &config,
            &encoder,
            Some(repository as Arc<dyn KeyframeRepository>),
        );
        let request = ferrofin_hls::CreateMainPlaylistRequest::new(
            Some(item),
            file,
            6000,
            120_000_000,
            "ts",
            "hls1/main/",
            "?static=true",
            remux,
        );
        generator
            .create_main_playlist(&request)
            .await
            .expect("playlist")
    }

    #[tokio::test]
    async fn a_remuxed_mkv_is_cut_on_its_keyframes() {
        let playlist = playlist_for("/media/movie.mkv", true, EncodingOptions::default()).await;

        // The first keyframe at or after 6 s is at 6.5 s; the rest runs to 12 s.
        assert_eq!(
            segment_lengths(&playlist),
            ["6.500000", "5.500000"],
            "{playlist}"
        );
    }

    #[rstest::rstest]
    #[case::a_transcode_makes_its_own_keyframes("/media/movie.mkv", true, false)]
    #[case::the_extension_is_not_allowed("/media/movie.mp4", true, true)]
    #[case::extraction_is_switched_off("/media/movie.mkv", false, true)]
    #[tokio::test]
    async fn other_playlists_use_equal_length_segments(
        #[case] file: &str,
        #[case] allow_mkv: bool,
        #[case] remux: bool,
    ) {
        let options = EncodingOptions {
            allow_on_demand_metadata_based_keyframe_extraction_for_extensions: if allow_mkv {
                vec!["mkv".to_owned()]
            } else {
                Vec::new()
            },
            ..EncodingOptions::default()
        };

        let playlist = playlist_for(file, remux, options).await;

        assert_eq!(
            segment_lengths(&playlist),
            ["6.000000", "6.000000"],
            "{playlist}"
        );
    }

    #[test]
    fn attachment_path_never_lets_a_traversal_file_name_leave_the_cache_folder() {
        // The name comes from the file's own `filename` tag; the path manager
        // keeps only its leaf, and a name with no leaf has no path at all (the
        // extractor falls back to the attachment's index).
        let (_, paths) = config_and_paths();
        let io = FfmpegAttachmentIo {
            path_manager: Arc::new(ferrofin_core::path_manager::FerrofinPathManager::new(paths)),
        };
        let id = "d37ecb9d75b0c0a8e9ecb0a864ec670e";
        let folder = io.attachment_folder_path(id).expect("guid folder");
        for evil in ["../../etc/passwd", "a/b.ttf", "..\\x.ttf"] {
            let path = io.attachment_path(id, evil).expect("a leaf");
            assert!(path.starts_with(&folder), "{evil} -> {path}");
            // A plain leaf: no separator after the folder, so `..` is inert.
            assert!(
                !path[folder.len() + 1..].contains(std::path::is_separator),
                "{evil} -> {path}"
            );
        }
        for leafless in ["a/..", "a/", "..", ""] {
            assert_eq!(io.attachment_path(id, leafless), None, "{leafless}");
        }
        assert_eq!(
            io.attachment_path(id, "font.ttf"),
            Some(format!("{folder}/font.ttf"))
        );
    }

    #[tokio::test]
    async fn resolver_finds_the_source_with_its_own_attachments() {
        let attachment = MediaAttachment {
            index: 1,
            file_name: Some("font.ttf".to_owned()),
            ..MediaAttachment::default()
        };
        let mut primary = source("abc");
        primary.media_attachments = vec![attachment];
        let alternate = source("alt");
        let resolver = MediaSourceManagerResolver {
            media_sources: Arc::new(FakeSources {
                sources: vec![primary, alternate],
                attachments: Vec::new(),
            }),
        };
        let outcome = resolver
            .resolve(Uuid::from_u128(1), "abc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("abc"));
        assert_eq!(outcome.media_attachments.len(), 1);
        // A merged alternate version resolves to ITS (empty) attachment list, never
        // the primary's.
        let alt = resolver
            .resolve(Uuid::from_u128(1), "ALT")
            .await
            .unwrap()
            .unwrap();
        assert!(
            alt.media_attachments.is_empty(),
            "{:?}",
            alt.media_attachments
        );
    }

    #[tokio::test]
    async fn resolver_missing_source_is_none() {
        let resolver = MediaSourceManagerResolver {
            media_sources: Arc::new(FakeSources {
                sources: vec![source("abc")],
                attachments: Vec::new(),
            }),
        };
        let outcome = resolver.resolve(Uuid::from_u128(1), "nope").await.unwrap();
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn dyn_media_encoder_delegates_every_method() {
        let inner: Arc<dyn MediaEncoder> = Arc::new(RecordingEncoder);
        let wrapped = DynMediaEncoder(inner);
        assert_eq!(wrapped.encoder_path(), "ffmpeg");
        assert_eq!(wrapped.probe_path(), "ffprobe");
        assert!(wrapped.set_ffmpeg_path().await.unwrap());
        let info_request = MediaInfoRequest {
            media_source: MediaSourceInfo::default(),
            extract_chapters: false,
            media_is_audio: false,
        };
        assert!(wrapped.get_media_info(&info_request).await.is_ok());
        assert_eq!(
            wrapped
                .extract_audio_image("/x.flac", Some(0))
                .await
                .unwrap(),
            "audio.jpg"
        );
        assert_eq!(
            wrapped
                .extract_video_image(
                    "/x.mkv",
                    "mkv",
                    &MediaSourceInfo::default(),
                    &MediaStream::default(),
                    None,
                    Some(0),
                )
                .await
                .unwrap(),
            "video.jpg"
        );
        assert_eq!(
            wrapped.get_input_argument("/x.mkv", &MediaSourceInfo::default()),
            "file:/x.mkv"
        );
        assert_eq!(wrapped.get_time_parameter(10), "-ss 10");
        assert!(wrapped.convert_image("a", "b").await.is_ok());
    }

    /// A stand-in ffmpeg: an executable shell script running `body`.
    #[cfg(unix)]
    fn fake_ffmpeg(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("ffmpeg");
        crate::test_exec::stub_script(&path, body);
        path.to_string_lossy().into_owned()
    }

    /// The real subtitle I/O over `ffmpeg_path`, with the given persisted
    /// `SubtitleExtractionTimeoutMinutes`.
    #[cfg(unix)]
    fn subtitle_io(dir: &std::path::Path, ffmpeg_path: String, minutes: i32) -> FfmpegSubtitleIo {
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            dir.join("data"),
            dir.join("log"),
            dir.join("config"),
            dir.join("cache"),
            dir.join("web"),
        ));
        let options = EncodingOptions {
            subtitle_extraction_timeout_minutes: minutes,
            ..EncodingOptions::default()
        };
        FfmpegSubtitleIo {
            path_manager: Arc::new(FakePathManager {
                root: dir.to_string_lossy().into_owned(),
            }),
            ffmpeg_path,
            config: Arc::new(FakeConfig(paths, options)),
            encoding_reader: EncodingOptionsReader::new("subtitle extraction"),
        }
    }

    /// A configured timeout of zero minutes expires at once: the still-running
    /// ffmpeg is killed and its half-written output deleted, as upstream's
    /// `RunSubtitleExtractionProcess` + `ExtractSubtitlesForFile` do.
    #[cfg(unix)]
    #[tokio::test]
    async fn subtitle_extraction_times_out_after_the_configured_minutes() {
        let tmp = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(tmp.path(), "exec sleep infinity");
        let output = tmp.path().join("cache/0.srt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "partial").unwrap();
        let output = output.to_string_lossy().into_owned();
        let io = subtitle_io(tmp.path(), ffmpeg, 0);
        let error = io
            .extract(
                &format!("-y -i in.mkv \"{output}\""),
                std::slice::from_ref(&output),
            )
            .await
            .unwrap_err();
        assert!(error.contains("timed out after 0 minute(s)"), "{error}");
        assert!(!std::path::Path::new(&output).exists());
    }

    /// A broken `encoding.json` extracts with the default timeout, and says so
    /// once with the file's path rather than once per extraction.
    #[cfg(unix)]
    #[tokio::test]
    async fn subtitle_extraction_warns_once_about_a_broken_encoding_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(
            tmp.path(),
            "for arg; do out=\"$arg\"; done; printf 1 > \"$out\"",
        );
        let mut io = subtitle_io(tmp.path(), ffmpeg, 1);
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            tmp.path().join("data"),
            tmp.path().join("log"),
            tmp.path().join("config"),
            tmp.path().join("cache"),
            tmp.path().join("web"),
        ));
        io.config = Arc::new(
            ferrofin_core::FerrofinServerConfigurationManager::load(paths)
                .await
                .unwrap(),
        );
        let file = tmp.path().join("config/users/named/encoding.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{ \"SubtitleExtractionTimeoutMinutes\": 1, }").unwrap();

        let logs = crate::test_logs::CapturedLogs::default();
        let guard = logs.install();
        for name in ["0.srt", "1.srt"] {
            let output = tmp.path().join(name).to_string_lossy().into_owned();
            io.extract(
                &format!("-y -i in.mkv \"{output}\""),
                std::slice::from_ref(&output),
            )
            .await
            .unwrap();
            assert!(std::path::Path::new(&output).is_file());
        }
        drop(guard);

        let warnings = logs.lines_at("WARN");
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(
            warnings[0].contains(&file.display().to_string()),
            "{warnings:#?}"
        );
    }

    /// Within the configured timeout the extraction completes normally.
    #[cfg(unix)]
    #[tokio::test]
    async fn subtitle_extraction_within_the_timeout_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(
            tmp.path(),
            "for arg; do out=\"$arg\"; done; printf 1 > \"$out\"",
        );
        let output = tmp.path().join("0.srt").to_string_lossy().into_owned();
        let io = subtitle_io(tmp.path(), ffmpeg, 1);
        io.extract(
            &format!("-y -i in.mkv \"{output}\""),
            std::slice::from_ref(&output),
        )
        .await
        .unwrap();
        assert!(std::path::Path::new(&output).is_file());
    }

    /// A negative timeout, or one past the longest .NET timer delay, is refused
    /// before ffmpeg starts, instead of upstream's `CancelAfter` throw that
    /// leaves the started ffmpeg running.
    #[cfg(unix)]
    #[rstest::rstest]
    #[case::negative(-1)]
    #[case::beyond_the_longest_wait(i32::MAX)]
    #[tokio::test]
    async fn invalid_subtitle_extraction_timeout_never_starts_ffmpeg(#[case] minutes: i32) {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("started");
        let ffmpeg = fake_ffmpeg(tmp.path(), &format!(": > \"{}\"", marker.display()));
        let output = tmp.path().join("0.srt").to_string_lossy().into_owned();
        let io = subtitle_io(tmp.path(), ffmpeg, minutes);
        let error = io
            .extract(
                &format!("-y -i in.mkv \"{output}\""),
                std::slice::from_ref(&output),
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("invalid SubtitleExtractionTimeoutMinutes"),
            "{error}"
        );
        assert!(!marker.exists());
    }

    /// How often the cancellation test re-checks the stand-in ffmpeg.
    #[cfg(target_os = "linux")]
    const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(10);

    /// How many [`CANCEL_POLL`] intervals the cancellation test waits for the
    /// stand-in ffmpeg to start, and then to die, before failing.
    #[cfg(target_os = "linux")]
    const CANCEL_POLL_ATTEMPTS: u32 = 500;

    /// Whether `pid` is gone or a zombie awaiting its reaper (killed).
    #[cfg(target_os = "linux")]
    fn process_has_exited(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            // The state follows the parenthesised command name.
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X'))
        })
    }

    /// Dropping the request future mid-extraction — a client disconnecting
    /// during a seek — kills ffmpeg and deletes its half-written output,
    /// as upstream's cancellation token does, so the truncated file is never
    /// served from the subtitle cache.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_cancelled_subtitle_extraction_kills_ffmpeg_and_deletes_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("pid");
        let ffmpeg = fake_ffmpeg(
            tmp.path(),
            &format!(
                "for arg; do out=\"$arg\"; done; printf partial > \"$out\"; \
                 echo $$ > \"{pid}.tmp\"; mv \"{pid}.tmp\" \"{pid}\"; exec sleep infinity",
                pid = pid_file.display()
            ),
        );
        let output = tmp.path().join("0.srt").to_string_lossy().into_owned();
        let io = subtitle_io(tmp.path(), ffmpeg, 1);
        let args = format!("-y -i in.mkv \"{output}\"");
        // Boxed, not stack-pinned: `drop` must drop the future itself.
        let mut extraction = Box::pin(io.extract(&args, std::slice::from_ref(&output)));

        // Drive the extraction until ffmpeg has written part of its output.
        let mut started = false;
        for _ in 0..CANCEL_POLL_ATTEMPTS {
            tokio::select! {
                result = &mut extraction => panic!("the extraction ended early: {result:?}"),
                () = tokio::time::sleep(CANCEL_POLL) => {}
            }
            if pid_file.is_file() {
                started = true;
                break;
            }
        }
        assert!(started, "the stand-in ffmpeg never started");
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        assert!(std::path::Path::new(&output).is_file());

        // The request is cancelled.
        drop(extraction);

        assert!(!std::path::Path::new(&output).exists());
        let mut killed = false;
        for _ in 0..CANCEL_POLL_ATTEMPTS {
            if process_has_exited(&pid) {
                killed = true;
                break;
            }
            tokio::time::sleep(CANCEL_POLL).await;
        }
        assert!(killed, "ffmpeg {pid} kept running after the cancellation");
    }

    /// Dropping the request future mid-dump (a client giving up on a font)
    /// kills ffmpeg and deletes the attachment it was writing, as upstream's
    /// cancellation does (`Kill(true)`, then the failure path's delete), so a
    /// truncated font is never served from the attachment cache.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_cancelled_attachment_extraction_kills_ffmpeg_and_deletes_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("pid");
        let output = tmp.path().join("font.ttf").to_string_lossy().into_owned();
        let ffmpeg = fake_ffmpeg(
            tmp.path(),
            &format!(
                "printf partial > \"{output}\"; \
                 echo $$ > \"{pid}.tmp\"; mv \"{pid}.tmp\" \"{pid}\"; exec sleep infinity",
                pid = pid_file.display()
            ),
        );
        let io = FfmpegAttachmentIo {
            path_manager: Arc::new(FakePathManager {
                root: tmp.path().to_string_lossy().into_owned(),
            }),
        };
        let args = format!("-dump_attachment:3 \"{output}\" -i in.mkv -t 0 -f null null");
        let outputs = [output.clone()];
        // Boxed, not stack-pinned: `drop` must drop the future itself.
        let mut dump = Box::pin(io.run_ffmpeg(&ffmpeg, &args, None, &outputs));

        let mut started = false;
        for _ in 0..CANCEL_POLL_ATTEMPTS {
            tokio::select! {
                result = &mut dump => panic!("the dump ended early: {result:?}"),
                () = tokio::time::sleep(CANCEL_POLL) => {}
            }
            if pid_file.is_file() {
                started = true;
                break;
            }
        }
        assert!(started, "the stand-in ffmpeg never started");
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        assert!(std::path::Path::new(&output).is_file());

        // The request is cancelled.
        drop(dump);

        assert!(!std::path::Path::new(&output).exists());
        let mut killed = false;
        for _ in 0..CANCEL_POLL_ATTEMPTS {
            if process_has_exited(&pid) {
                killed = true;
                break;
            }
            tokio::time::sleep(CANCEL_POLL).await;
        }
        assert!(killed, "ffmpeg {pid} kept running after the cancellation");
    }

    /// A dump that runs to its end keeps what it wrote, whatever its exit code:
    /// deleting a failed run's outputs is the extractor's job.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_finished_attachment_extraction_keeps_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("font.ttf").to_string_lossy().into_owned();
        let ffmpeg = fake_ffmpeg(tmp.path(), &format!("printf font > \"{output}\"; exit 1"));
        let io = FfmpegAttachmentIo {
            path_manager: Arc::new(FakePathManager {
                root: tmp.path().to_string_lossy().into_owned(),
            }),
        };
        let outputs = [output.clone()];
        let code = io
            .run_ffmpeg(&ffmpeg, "-i in.mkv", None, &outputs)
            .await
            .unwrap();
        assert_eq!(code, 1);
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "font");
    }

    #[tokio::test]
    async fn ffmpeg_attachment_io_paths_and_fs() {
        let tmp = tempfile::tempdir().unwrap();
        let io = FfmpegAttachmentIo {
            path_manager: Arc::new(FakePathManager {
                root: tmp.path().to_string_lossy().into_owned(),
            }),
        };
        // Folder/path derive from the PathManager.
        assert_eq!(
            io.attachment_folder_path("src"),
            Some(format!("{}/src", tmp.path().to_string_lossy()))
        );
        assert!(
            io.attachment_path("src", "font.ttf")
                .is_some_and(|path| path.ends_with("/src/font.ttf"))
        );

        // file_exists + read_file + remove_file over a real temp file.
        let file = tmp.path().join("data.bin");
        assert!(!io.file_exists(&file.to_string_lossy()));
        std::fs::write(&file, b"hello").unwrap();
        assert!(io.file_exists(&file.to_string_lossy()));
        assert_eq!(
            io.read_file(&file.to_string_lossy()).await.unwrap(),
            b"hello"
        );
        assert!(io.read_file("/no/such/file").await.is_err());
        io.remove_file(&file.to_string_lossy()).await.unwrap();
        assert!(!io.file_exists(&file.to_string_lossy()));
        // Already gone is not an error: the failed run may never have
        // written it.
        io.remove_file(&file.to_string_lossy()).await.unwrap();
    }

    /// The attachment dump spawns ffmpeg with exactly the argv the builder
    /// quoted. The old hand-rolled splitter here toggled on every `"` and kept
    /// the `\` escapes, so a media path containing a quote reached ffmpeg
    /// mangled and fonts were never extracted.
    #[cfg(unix)]
    #[tokio::test]
    async fn attachment_dump_gets_a_quoted_path_back_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let record = tmp.path().join("argv");
        let ffmpeg = fake_ffmpeg(
            tmp.path(),
            &format!("printf '%s\\n' \"$@\" > \"{}\"", record.display()),
        );
        let io = FfmpegAttachmentIo {
            path_manager: Arc::new(FakePathManager {
                root: tmp.path().to_string_lossy().into_owned(),
            }),
        };
        let media = r#"/media/The "Best" \ Movie.mkv"#;
        let args = format!(
            "-dump_attachment:3 {} -i {} -t 0 -f null null",
            ferrofin_mediaencoding::encoder::quote("/cache/att/font.ttf"),
            ferrofin_mediaencoding::encoder::get_input_argument("file", media, MediaProtocol::File),
        );
        assert_eq!(io.run_ffmpeg(&ffmpeg, &args, None, &[]).await.unwrap(), 0);
        let recorded = std::fs::read_to_string(&record).unwrap();
        assert_eq!(
            recorded.lines().collect::<Vec<_>>(),
            [
                "-dump_attachment:3",
                "/cache/att/font.ttf",
                "-i",
                &format!("file:{media}"),
                "-t",
                "0",
                "-f",
                "null",
                "null",
            ]
        );
        // Unbalanced quoting is refused rather than guessed at.
        assert!(
            io.run_ffmpeg(&ffmpeg, r#"-i "unterminated"#, None, &[])
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_pause_key_probe_runs_only_for_a_validated_ffmpeg() {
        use ferrofin_mediaencoding::transcoding::throttler::{PauseKeySlot, PauseKeys};
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ffmpeg");
        // Prints Jellyfin-ffmpeg's pause-key help once it has read `?q` on stdin.
        crate::test_exec::stub_script(
            &script,
            "keys=$(dd bs=1 count=2 2>/dev/null)\n\
             if [ \"$keys\" = '?q' ]; then echo 'p      pause transcoding' >&2; fi\n",
        );
        let paths = |validated| FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: script.clone(),
            ffprobe: "ffprobe".into(),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::builder()
                .ffmpeg_version(ferrofin_mediaencoding::FfmpegVersion::new(8, 0))
                .validated(validated)
                .build(),
            chromaprint_muxer: false,
        };
        let (skipped, probed) = (PauseKeySlot::default(), PauseKeySlot::default());
        let handle = tokio::runtime::Handle::current();
        spawn_pause_key_probe(&handle, &paths(false), skipped.clone());
        spawn_pause_key_probe(&handle, &paths(true), probed.clone());
        for _ in 0..500 {
            if probed.get().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(probed.get(), Some(PauseKeys::Patched));
        // Give a wrongly spawned probe time to land too.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(skipped.get(), None, "a placeholder ffmpeg is never run");
    }
}
