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
use ferrofin_hls::{DynamicHlsPlaylistGenerator, HlsStreamManagerImpl};
use ferrofin_mediaencoding::attachments::{AttachmentIo, MediaSourceResolver};
use ferrofin_mediaencoding::subtitles::{
    SubtitleEditParser, SubtitleEncoder as PureSubtitleEncoder, SubtitleEncoderImpl, SubtitleIo,
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
use ferrofin_traits::system::PathManager;
use uuid::Uuid;

use crate::bootstrap::FfmpegPaths;
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
    /// Supplies the item's trickplay tile resolutions for the master playlist.
    pub trickplay: Option<Arc<dyn ferrofin_traits::trickplay::TrickplayManager>>,
    /// Releases the live stream a killed job was reading.
    pub sessions: Option<Arc<dyn ferrofin_traits::session::SessionManager>>,
}

/// The one thing job teardown needs from the session layer: release a live
/// stream no session still wants (`ISessionManager.CloseLiveStreamIfNeededAsync`).
#[async_trait]
trait LiveStreamReleaser: Send + Sync {
    /// Closes `live_stream_id` unless another session is still using it.
    async fn release(&self, live_stream_id: &str, session_id: &str) -> Result<(), ServiceError>;
}

#[async_trait]
impl LiveStreamReleaser for Arc<dyn ferrofin_traits::session::SessionManager> {
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
/// `report_progress` is a no-op, and deliberately says so: upstream's
/// `TranscodeManager.ReportTranscodingProgress` builds a `TranscodingInfo` from
/// the encoding state and pushes it into the session, but nothing in Ferrofin
/// produces progress in the first place (no ffmpeg progress reader), so
/// `SessionInfo.TranscodingInfo` stays null and the dashboard shows no transcode
/// details. Building that reader is its own piece of work — see the ledger — and
/// this is the seam it will report through.
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
/// The `NoopSessionReporter` is used for job teardown (progress → session-layer
/// reporting is deferred; killed-job partial-file cleanup is handled by the
/// manager's `FsFileCleaner`).
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
    };
    let pure_encoder = Arc::new(PureSubtitleEncoder::new(SubtitleEditParser::new(), sub_io));
    let subtitles: Arc<dyn SubtitleEncoder> =
        Arc::new(SubtitleEncoderImpl::new(pure_encoder, sub_resolver));

    // ---- HLS transcode chain (below the planner seam) ---------------------
    let planner = FerrofinStreamStatePlanner::new(
        Arc::clone(&media_sources),
        Arc::clone(&encoder),
        EncodingHelper::new(ffmpeg.capabilities.clone()),
        config,
        Arc::clone(&paths),
        Arc::clone(&subtitles),
        ffmpeg.supports_filter("tonemapx"),
    );
    let planner = match library {
        Some(library) => planner.with_library(library),
        None => planner,
    };
    let transcoder = TokioSegmentTranscoder::new();
    // A live transcode holds a tuner: the reporter releases it when the job dies.
    let reporter: Arc<dyn ferrofin_mediaencoding::SessionReporter> = match extras.sessions {
        Some(sessions) => Arc::new(LiveStreamReleasingReporter { releaser: sessions }),
        None => Arc::new(NoopSessionReporter),
    };
    let manager = Arc::new(TranscodeManagerImpl::new(reporter));
    // The idle reaper kills any transcode whose consumers vanished without a
    // stop report (a disconnected cast receiver, a killed browser tab): no
    // active segment request and no session ping within the job's ping timeout
    // → the job dies and its partial files are cleaned up. Spawned only when a
    // runtime exists so the sync composition unit test can still call this.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(
            Arc::clone(&manager)
                .run_idle_reaper(std::time::Duration::from_secs(IDLE_REAPER_SWEEP_SECS)),
        );
    }
    // The generator reads live encoding options per request; First-Light returns
    // the defaults (the persisted named-config accessor is not yet threaded
    // through `ServerConfigurationManager`).
    let generator_config: Box<dyn Fn() -> EncodingOptions + Send + Sync> =
        Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(
        generator_config,
        Vec::new(),
    ));
    let mut hls_impl = HlsStreamManagerImpl::new(
        planner,
        transcoder,
        manager,
        generator,
        paths as Arc<dyn ferrofin_traits::system::ServerApplicationPaths>,
    );
    // Without this the master playlist lists no `#EXT-X-IMAGE-STREAM-INF`, so a
    // client sees no trickplay tiles however many the library holds.
    if let Some(trickplay) = extras.trickplay {
        hls_impl = hls_impl.with_trickplay(trickplay);
    }
    let hls: Arc<dyn HlsStreamManager> = Arc::new(hls_impl);

    // ---- attachment extractor (real ffmpeg + filesystem) ------------------
    let resolver = Arc::new(MediaSourceManagerResolver { media_sources });
    let io = Arc::new(FfmpegAttachmentIo { path_manager });
    // `AttachmentExtractorImpl<E, …>` holds an `Arc<E>` with `E: Sized`, so the
    // trait-object encoder is wrapped in the sized [`DynMediaEncoder`] newtype.
    let attachments: Arc<dyn AttachmentExtractor> = Arc::new(AttachmentExtractorImpl::new(
        Arc::new(DynMediaEncoder(encoder)),
        resolver,
        io,
    ));

    (hls, attachments, subtitles)
}

/// The real ffmpeg + filesystem [`SubtitleIo`] for the subtitle encoder.
///
/// Mirrors [`FfmpegAttachmentIo`]: cache paths come from the [`PathManager`],
/// file/HTTP reads use `tokio::fs`/`reqwest`, and extraction runs the injected
/// ffmpeg binary through `tokio::process`.
struct FfmpegSubtitleIo {
    path_manager: Arc<dyn PathManager>,
    ffmpeg_path: String,
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
        let parts = split_ffmpeg_args(args);
        let status = tokio::process::Command::new(&self.ffmpeg_path)
            .args(&parts)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| format!("spawn ffmpeg {}: {e}", self.ffmpeg_path))?;
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

    fn attachment_path(&self, media_source_id: &str, file_name: &str) -> String {
        // The path manager refuses a file name that is not a plain leaf
        // (`..`, separators). Such a name came from the file's own `filename`
        // tag, so it must never reach the command line as-is: fall back to a
        // sanitized leaf inside the cache folder rather than the raw name.
        self.path_manager
            .attachment_path(media_source_id, file_name)
            .or_else(|| {
                let leaf: String = file_name
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                self.path_manager
                    .attachment_path(media_source_id, leaf.trim_matches('.'))
            })
            .unwrap_or_else(|| file_name.to_owned())
    }

    fn file_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
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
    ) -> Result<i32, String> {
        // `args` is a pre-built ffmpeg command line (a single space-joined
        // string, matching the C# `arguments` string); split it into parts while
        // honouring the double-quoted attachment-output path.
        let parts = split_ffmpeg_args(args);
        let mut command = tokio::process::Command::new(ffmpeg_path);
        if let Some(dir) = working_dir {
            command.current_dir(dir);
        }
        let status = command
            .args(&parts)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| format!("spawn ffmpeg {ffmpeg_path}: {e}"))?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// Splits a space-joined ffmpeg argument string into parts, keeping
/// double-quoted spans (e.g. the `-dump_attachment` output path) as one token
/// with the surrounding quotes stripped.
fn split_ffmpeg_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    for ch in args.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(current);
    }
    out
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
        async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
            Ok(())
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
            _media_path: &str,
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
        ) -> String {
            String::new()
        }
        fn extracted_data_paths(&self, _item_id: Uuid, _media_path: &str) -> Vec<String> {
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
        let config: Arc<dyn ServerConfigurationManager> = Arc::new(FakeConfig(Arc::clone(&paths)));
        (config, paths)
    }

    /// A fake [`ServerConfigurationManager`] exposing only the paths.
    struct FakeConfig(Arc<FerrofinServerApplicationPaths>);

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
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
        let (config, paths) = config_and_paths();
        let path_manager: Arc<dyn PathManager> = Arc::new(FakePathManager {
            root: "/cache/att".to_owned(),
        });
        // The trio builds without panicking; every slot is a real trait object.
        let ffmpeg = FfmpegPaths {
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
                trickplay: None,
                sessions: None,
            },
        );
    }

    /// Records what job teardown asked the session layer to release.
    #[derive(Default)]
    struct RecordingReleaser {
        released: std::sync::Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    #[async_trait]
    impl LiveStreamReleaser for RecordingReleaser {
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
        let (config, paths) = config_and_paths();
        let path_manager: Arc<dyn PathManager> = Arc::new(FakePathManager {
            root: "/cache/att".to_owned(),
        });
        let ffmpeg = FfmpegPaths {
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
                trickplay: Some(Arc::new(FakeTrickplay)),
                sessions: None,
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

    #[test]
    fn attachment_path_never_lets_a_traversal_file_name_leave_the_cache_folder() {
        // The name comes from the file's own `filename` tag; the path manager
        // refuses anything but a plain leaf, and the fallback must stay inside
        // the cache folder rather than hand the raw name to ffmpeg.
        let (_, paths) = config_and_paths();
        let io = FfmpegAttachmentIo {
            path_manager: Arc::new(ferrofin_core::path_manager::FerrofinPathManager::new(paths)),
        };
        let id = "d37ecb9d75b0c0a8e9ecb0a864ec670e";
        let folder = io.attachment_folder_path(id).expect("guid folder");
        for evil in ["../../etc/passwd", "a/b.ttf", "..\\x.ttf"] {
            let path = io.attachment_path(id, evil);
            assert!(path.starts_with(&folder), "{evil} -> {path}");
            // A plain leaf: no separator after the folder, so `..` is inert.
            assert!(
                !path[folder.len() + 1..].contains(['/', '\\']),
                "{evil} -> {path}"
            );
        }
        assert_eq!(
            io.attachment_path(id, "font.ttf"),
            format!("{folder}/font.ttf")
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
        assert!(alt.media_attachments.is_empty());
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
                .ends_with("/src/font.ttf")
        );

        // file_exists + read_file over a real temp file.
        let file = tmp.path().join("data.bin");
        assert!(!io.file_exists(&file.to_string_lossy()));
        std::fs::write(&file, b"hello").unwrap();
        assert!(io.file_exists(&file.to_string_lossy()));
        assert_eq!(
            io.read_file(&file.to_string_lossy()).await.unwrap(),
            b"hello"
        );
        assert!(io.read_file("/no/such/file").await.is_err());
    }

    #[test]
    fn split_ffmpeg_args_keeps_quoted_path_as_one_token() {
        let args =
            r#"-dump_attachment:3 "/cache/att/font.ttf" -i file:/media/x.mkv -t 0 -f null null"#;
        let parts = split_ffmpeg_args(args);
        assert_eq!(parts[0], "-dump_attachment:3");
        assert_eq!(parts[1], "/cache/att/font.ttf");
        assert_eq!(parts[2], "-i");
        assert_eq!(parts[3], "file:/media/x.mkv");
        assert_eq!(parts.last().unwrap(), "null");
    }

    #[test]
    fn split_ffmpeg_args_handles_plain_tokens() {
        let parts = split_ffmpeg_args("-i input.mkv -c copy");
        assert_eq!(parts, vec!["-i", "input.mkv", "-c", "copy"]);
    }

    #[test]
    fn split_ffmpeg_args_empty_is_empty() {
        assert!(split_ffmpeg_args("   ").is_empty());
    }
}
