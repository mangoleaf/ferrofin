//! Media-encoding **service** traits — the ffmpeg seam.
//!
//! Ports of `IMediaEncoder`, `ITranscodeManager`, `ISubtitleEncoder`, and
//! `IAttachmentExtractor` in `MediaBrowser.Controller.MediaEncoding`. These wrap
//! the ffmpeg/ffprobe subprocess: probing media, extracting frames and
//! subtitles and attachments, and managing live transcode jobs.
//!
//! Port rules applied:
//! - Item **identity** arguments become [`uuid::Uuid`]; the domain `BaseItem`
//!   receivers of `GetSubtitles`/`GetAttachment` become an [`item_id`](Uuid).
//! - `MediaSource` / `MediaStream` / `MediaAttachment` value types are reused
//!   from `ferrofin-model` ([`MediaSourceInfo`]/[`MediaStream`]/[`MediaAttachment`]).
//! - `Task<T>` becomes `async fn -> Result<T, ServiceError>`; `Task` becomes
//!   `Result<(), ServiceError>`; `CancellationToken`/`IProgress`/`ProcessPriorityClass`
//!   are dropped for v1.
//! - Streams (`Task<Stream>`) become `Result<Vec<u8>, ServiceError>` — the trait
//!   returns the produced bytes rather than leaking `std::io::Stream` shapes.
//! - The deeply ffmpeg-internal hardware-capability probes
//!   (`IsVaapiDevice*`/`SupportsEncoder`/…) and the `EncodingJobInfo`/`StreamState`
//!   coupled command-building methods are **not** ported here: they depend on
//!   un-ported encoding-state structs and belong to the `ferrofin-core` encoder
//!   implementation (Wave 6), not the DI seam.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion.
//!
//! Port note: `IMediaEncoder.GetMediaInfo` returns a rich `MediaInfo` probe
//! result. [`MediaEncoder::get_media_info`] returns the narrower
//! [`MediaSourceInfo`] (streams/container) most callers need;
//! [`MediaEncoder::get_media_info_full`] returns the full
//! [`MediaInfo`](ferrofin_model::media_info::MediaInfo) including the embedded
//! metadata the library scan reads from audio tags.

use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::Video3DFormat;
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::media_info::MediaInfo;
use uuid::Uuid;

use crate::error::ServiceError;

/// A parked media-probe request: which file to probe and how.
///
/// Port of `MediaEncoding/MediaInfoRequest.cs` reduced to the fields the seam
/// needs: the media source and whether the item is audio-only (the C# type also
/// carries an `ExtractChapters` flag, kept here).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfoRequest {
    /// The media source to probe.
    pub media_source: MediaSourceInfo,
    /// Whether chapter markers should be extracted during the probe.
    pub extract_chapters: bool,
    /// Whether the item is known to be audio-only (skips video-stream probing).
    pub media_is_audio: bool,
}

/// Distinguishes the kind of transcode a job is performing.
///
/// Port of `MediaEncoding/TranscodingJobType.cs`. Not a wire type — it is
/// service-internal state, so it lives here rather than in `ferrofin-model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscodingJobType {
    /// A progressive (single-file) transcode.
    Progressive,
    /// An HLS (segmented) transcode.
    Hls,
    /// A DASH (segmented) transcode.
    Dash,
}

/// A handle to a running transcode job.
///
/// Port of the identity/reporting subset of `MediaEncoding/TranscodingJob.cs`.
/// The full C# type owns the ffmpeg `Process` and mutable progress counters;
/// the seam only needs to *name* a job (by play-session id, path, and type) and
/// report its current progress, so this carries just that identifying surface.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscodingJobHandle {
    /// The playback-session id this job serves, if any.
    pub play_session_id: Option<String>,
    /// The output path of the transcoded file.
    pub path: String,
    /// The kind of transcode.
    pub job_type: TranscodingJobType,
    /// The id of the device the job is streaming to, if known.
    pub device_id: Option<String>,
}

/// A snapshot of a transcode's progress, reported to the session layer.
///
/// Port of the `ReportTranscodingProgress` argument bundle; grouped into one
/// struct so the trait method stays object-safe and readable.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TranscodingProgress {
    /// The current transcoding position, in ticks (100 ns units).
    pub position_ticks: Option<i64>,
    /// The current output framerate.
    pub framerate: Option<f32>,
    /// The completion percentage (0.0–100.0).
    pub percent_complete: Option<f64>,
    /// The number of bytes transcoded so far.
    pub bytes_transcoded: Option<i64>,
    /// The current output bitrate, in bits per second.
    pub bit_rate: Option<i32>,
}

/// Wraps the ffmpeg/ffprobe subprocess: probing, frame and image extraction.
///
/// Port of the object-safe, domain-tree-free subset of `IMediaEncoder`. The
/// hardware-capability probes and `EncodingJobInfo`-coupled command builders are
/// deferred to the Wave 6 implementation (see the module docs).
#[async_trait]
pub trait MediaEncoder: Send + Sync {
    /// The resolved path to the `ffmpeg` binary. Port of the `EncoderPath`
    /// property.
    fn encoder_path(&self) -> String;

    /// The resolved path to the `ffprobe` binary. Port of the `ProbePath`
    /// property.
    fn probe_path(&self) -> String;

    /// Locates and validates the ffmpeg binary, returning whether a valid one
    /// was found. Port of `SetFFmpegPath()`.
    async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError>;

    /// Probes a media source, returning its container/stream information. Port
    /// of `GetMediaInfo(MediaInfoRequest, …)`; returns [`MediaSourceInfo`]
    /// pending a ported `MediaInfo` (see the module docs).
    async fn get_media_info(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError>;

    /// Probes a media source and returns the **full** [`MediaInfo`] — the
    /// container/streams *plus* the embedded metadata the normalizer extracts
    /// (album/artists/album-artists/genres/track/disc/year + MusicBrainz ids for
    /// audio). The scanner uses this to enrich music items from their tags.
    ///
    /// The default wraps [`get_media_info`](Self::get_media_info) with no
    /// embedded metadata (so fakes keep working); the ffmpeg-backed encoder
    /// overrides it to return the normalizer's full result.
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
            ..MediaInfo::default()
        })
    }

    /// Extracts an embedded cover image from an audio file, returning the path
    /// to the written image. Port of `ExtractAudioImage(path, imageStreamIndex,
    /// …)`.
    async fn extract_audio_image(
        &self,
        path: &str,
        image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError>;

    /// Extracts a single frame from a video as an image, returning the written
    /// path. Port of `ExtractVideoImage(inputFile, container, mediaSource,
    /// videoStream, threedFormat, offset, …)`; `offset` is in ticks.
    async fn extract_video_image(
        &self,
        input_file: &str,
        container: &str,
        media_source: &MediaSourceInfo,
        video_stream: &MediaStream,
        threed_format: Option<Video3DFormat>,
        offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError>;

    /// Builds the ffmpeg input argument for a single file. Port of
    /// `GetInputArgument(string inputFile, MediaSourceInfo)`.
    fn get_input_argument(&self, input_file: &str, media_source: &MediaSourceInfo) -> String;

    /// Formats a tick count as an ffmpeg `-ss`-style time parameter. Port of
    /// `GetTimeParameter(long ticks)`.
    fn get_time_parameter(&self, ticks: i64) -> String;

    /// Converts an image from one file/format to another. Port of
    /// `ConvertImage(inputPath, outputPath)`.
    async fn convert_image(&self, input_path: &str, output_path: &str) -> Result<(), ServiceError>;
}

/// Compile-time assertion that [`MediaEncoder`] is object-safe.
fn _assert_object_safe_media_encoder(_: &dyn MediaEncoder) {}

/// Extracts evenly spaced trickplay thumbnail frames from a video file.
///
/// Port of the software path of
/// `IMediaEncoder.ExtractVideoImagesOnIntervalAccelerated`: one ffmpeg run with
/// an `fps=1/interval` + width-bounded `scale` filter chain writing numbered
/// JPEG thumbnails into a directory. The hardware-acceleration and
/// keyframe-only variants ride the deferred hw-accel matrix (see the
/// `ferrofin-mediaencoding` crate docs) and are not modeled at this seam; the
/// trickplay manager's `EnableHwAcceleration`/`EnableHwEncoding`/
/// `EnableKeyFrameOnlyExtraction` options therefore select the software path.
///
/// This is a separate small trait (rather than a `MediaEncoder` method) so the
/// trickplay manager in `ferrofin-core` can depend on exactly this capability.
#[async_trait]
pub trait TrickplayFrameExtractor: Send + Sync {
    /// Extracts one thumbnail every `interval_ms` milliseconds from
    /// `input_path`, scaled to at most `max_width` (forced even) pixels wide
    /// with the display aspect ratio preserved, writing numbered JPEGs into
    /// `output_dir` (created if absent) and returning their paths sorted by
    /// file name.
    ///
    /// `qscale` is the ffmpeg `-qscale:v` quality (1 best – 31 worst, clamped);
    /// `threads` is the ffmpeg thread count (`0` lets ffmpeg decide).
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when ffmpeg cannot be run or produces no
    /// frames, or when `interval_ms`/`max_width` are not positive.
    async fn extract_trickplay_frames(
        &self,
        input_path: &str,
        interval_ms: i32,
        max_width: i32,
        qscale: i32,
        threads: i32,
        output_dir: &str,
    ) -> Result<Vec<String>, ServiceError>;
}

/// Compile-time assertion that [`TrickplayFrameExtractor`] is object-safe.
fn _assert_object_safe_trickplay_frame_extractor(_: &dyn TrickplayFrameExtractor) {}

/// Manages live transcode jobs: lookup, keep-alive pings, progress, teardown.
///
/// Port of the object-safe subset of `ITranscodeManager`. `StartFfMpeg` (which
/// takes an un-ported `StreamState` + `CancellationTokenSource`) and the
/// `LockAsync` disposable are deferred to the Wave 6 implementation.
#[async_trait]
pub trait TranscodeManager: Send + Sync {
    /// Looks up a running job by its playback-session id. Port of the
    /// `GetTranscodingJob(string playSessionId)` overload.
    async fn get_transcoding_job_by_session(
        &self,
        play_session_id: &str,
    ) -> Result<Option<TranscodingJobHandle>, ServiceError>;

    /// Looks up a running job by its output path and type. Port of the
    /// `GetTranscodingJob(string path, TranscodingJobType)` overload.
    async fn get_transcoding_job_by_path(
        &self,
        path: &str,
        job_type: TranscodingJobType,
    ) -> Result<Option<TranscodingJobHandle>, ServiceError>;

    /// Keeps a job alive (resetting its idle timer), optionally recording that
    /// the user paused it. Port of `PingTranscodingJob(playSessionId,
    /// isUserPaused)`.
    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError>;

    /// Kills the transcode jobs for a device (optionally scoped to one play
    /// session). Port of `KillTranscodingJobs(deviceId, playSessionId,
    /// deleteFiles)`; the `deleteFiles` predicate is replaced by a flag.
    async fn kill_transcoding_jobs(
        &self,
        device_id: &str,
        play_session_id: Option<&str>,
        delete_files: bool,
    ) -> Result<(), ServiceError>;

    /// Reports a job's current progress to the session layer. Port of
    /// `ReportTranscodingProgress(job, state, …)`; the progress arguments are
    /// bundled into [`TranscodingProgress`].
    async fn report_transcoding_progress(
        &self,
        job: &TranscodingJobHandle,
        progress: TranscodingProgress,
    ) -> Result<(), ServiceError>;

    /// Called when a client begins requesting a transcode output, returning the
    /// (existing) job serving that path. Port of `OnTranscodeBeginRequest(path,
    /// type)`.
    async fn on_transcode_begin_request(
        &self,
        path: &str,
        job_type: TranscodingJobType,
    ) -> Result<Option<TranscodingJobHandle>, ServiceError>;

    /// Called when a transcode output is finished with, allowing teardown. Port
    /// of `OnTranscodeEndRequest(job)`.
    async fn on_transcode_end_request(
        &self,
        job: &TranscodingJobHandle,
    ) -> Result<(), ServiceError>;
}

/// Compile-time assertion that [`TranscodeManager`] is object-safe.
fn _assert_object_safe_transcode_manager(_: &dyn TranscodeManager) {}

/// Extracts and converts subtitle tracks via ffmpeg.
///
/// Port of `ISubtitleEncoder`. The domain `BaseItem` receiver of `GetSubtitles`
/// becomes an [`item_id`](Uuid); the produced `Stream` becomes the subtitle
/// bytes.
#[async_trait]
pub trait SubtitleEncoder: Send + Sync {
    /// Gets a subtitle track converted to `output_format`, as bytes, for the
    /// given time window. Port of `GetSubtitles(item, mediaSourceId,
    /// subtitleStreamIndex, outputFormat, startTimeTicks, endTimeTicks,
    /// preserveOriginalTimestamps, …)`.
    #[allow(clippy::too_many_arguments)]
    async fn get_subtitles(
        &self,
        item_id: Uuid,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_format: &str,
        start_time_ticks: i64,
        end_time_ticks: i64,
        preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, ServiceError>;

    /// Detects the character set of an external subtitle stream. Port of
    /// `GetSubtitleFileCharacterSet(subtitleStream, language, mediaSource, …)`.
    async fn get_subtitle_file_character_set(
        &self,
        subtitle_stream: &MediaStream,
        language: &str,
        media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError>;

    /// Resolves (extracting if needed) the on-disk path of a subtitle track.
    /// Port of `GetSubtitleFilePath(subtitleStream, mediaSource, …)`.
    async fn get_subtitle_file_path(
        &self,
        subtitle_stream: &MediaStream,
        media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError>;

    /// Extracts every extractable (text and PGS) subtitle from a source. Port
    /// of `ExtractAllExtractableSubtitles(mediaSource, …)`.
    async fn extract_all_extractable_subtitles(
        &self,
        media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError>;
}

/// Compile-time assertion that [`SubtitleEncoder`] is object-safe.
fn _assert_object_safe_subtitle_encoder(_: &dyn SubtitleEncoder) {}

/// An extracted attachment: its metadata plus the extracted bytes.
///
/// Port of the C# `(MediaAttachment Attachment, Stream Stream)` tuple returned
/// by `IAttachmentExtractor.GetAttachment`; the `Stream` becomes the bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedAttachment {
    /// The attachment's stream metadata.
    pub attachment: MediaAttachment,
    /// The extracted attachment bytes.
    pub data: Vec<u8>,
}

/// Extracts embedded font/cover attachments from media containers.
///
/// Port of `IAttachmentExtractor`. The domain `BaseItem` receiver of
/// `GetAttachment` becomes an [`item_id`](Uuid).
#[async_trait]
pub trait AttachmentExtractor: Send + Sync {
    /// Extracts one attachment (by index) from an item's media source. Port of
    /// `GetAttachment(item, mediaSourceId, attachmentStreamIndex, …)`.
    async fn get_attachment(
        &self,
        item_id: Uuid,
        media_source_id: &str,
        attachment_stream_index: i32,
    ) -> Result<ExtractedAttachment, ServiceError>;

    /// Extracts every attachment from a media source to the on-disk cache. Port
    /// of `ExtractAllAttachments(inputFile, mediaSource, …)`.
    async fn extract_all_attachments(
        &self,
        input_file: &str,
        media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError>;
}

/// Compile-time assertion that [`AttachmentExtractor`] is object-safe.
fn _assert_object_safe_attachment_extractor(_: &dyn AttachmentExtractor) {}

/// A parked HLS/transcode streaming request — the query-string surface the
/// `DynamicHls`/`Videos`/`UniversalAudio` controllers accept, reduced to the
/// fields the software transcode path needs.
///
/// Port of the shared `StreamingRequestDto`/`VideoRequestDto` fields consumed by
/// `StreamingHelpers.GetStreamingState`. The full C# DTO carries the entire
/// device-profile/codec/bitrate matrix; the fields that drive **which file to
/// transcode, into what container, at what segment length, for which
/// session/device** are carried here, and the rest of the matrix is resolved
/// from server defaults by the implementation (see `ferrofin-mediaencoding`). The
/// raw `query_string` is preserved verbatim so the generated playlist's segment
/// URLs carry the client's parameters forward (the C# `Request.QueryString`).
#[derive(Debug, Clone, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "faithful field-for-field port of the C# StreamingRequestDto flags"
)]
pub struct HlsStreamRequest {
    /// The item being streamed (`itemId`).
    pub item_id: Uuid,
    /// The chosen media source id (`mediaSourceId`), if the client pinned one.
    pub media_source_id: Option<String>,
    /// The playback-session id this stream belongs to (`playSessionId`).
    pub play_session_id: Option<String>,
    /// The requesting device id (`deviceId`) — the kill/keep-alive scope.
    pub device_id: Option<String>,
    /// The desired segment container, e.g. `"ts"` or `"mp4"` (`segmentContainer`).
    pub segment_container: Option<String>,
    /// The desired segment length in seconds (`segmentLength`).
    pub segment_length: Option<i32>,
    /// The desired output audio codec (`audioCodec`).
    pub audio_codec: Option<String>,
    /// The desired output video codec (`videoCodec`).
    pub video_codec: Option<String>,
    /// The transcoding profile's audio-channel cap (`transcodingMaxAudioChannels`).
    ///
    /// Forwarded from the PlaybackInfo-negotiated transcode URL; drives the
    /// ffmpeg `-ac` downmix so a >2ch source doesn't produce AAC the browser's
    /// MSE pipeline can't decode.
    pub transcoding_max_audio_channels: Option<i32>,
    /// The negotiated video bitrate cap in bit/s (`videoBitRate`) — drives the
    /// encoder's `-maxrate`/`-bufsize` and the bitrate-driven downscale.
    pub video_bitrate: Option<i32>,
    /// The negotiated audio bitrate cap in bit/s (`audioBitRate`).
    pub audio_bitrate: Option<i32>,
    /// The maximum output width in pixels (`maxWidth`) — bounds the scale
    /// filter on re-encode.
    pub max_width: Option<i32>,
    /// The maximum output height in pixels (`maxHeight`).
    pub max_height: Option<i32>,
    /// The maximum output framerate (`maxFramerate`), applied on re-encode when
    /// the source exceeds it.
    pub max_framerate: Option<f32>,
    /// Whether `-c:v copy` is permitted (`allowVideoStreamCopy`; default true).
    /// PlaybackInfo appends `allowVideoStreamCopy=false` to the transcode URL
    /// when the client forbade it.
    pub allow_video_stream_copy: bool,
    /// Whether `-c:a copy` is permitted (`allowAudioStreamCopy`; default true).
    pub allow_audio_stream_copy: bool,
    /// Whether the client asked for a static (direct) stream (`static`).
    pub is_static: bool,
    /// The resume offset in ticks (`startTimeTicks`), when the client is resuming.
    ///
    /// Threaded from the PlaybackInfo transcode URL through the playlist into its
    /// baked-in init/segment URLs. Only the fMP4 init serve consults it: it starts
    /// the transcode at the segment containing this offset so the init header the
    /// client caches carries the same moov edit list as the seek-offset segments
    /// it then plays. Without this, a from-segment-0 init pairs with seek-offset
    /// segments and the player maps the media back to t≈0 and stalls on a resume.
    pub start_time_ticks: Option<i64>,
    /// The requested video profile (`profile`), e.g. `high` — the
    /// `CODECS` profile byte of a re-encode.
    pub profile: Option<String>,
    /// The requested video level (`level`), e.g. `41` — the `CODECS` level of
    /// a re-encode (clamped by `NormalizeTranscodingLevel`).
    pub level: Option<String>,
    /// The requested output framerate (`framerate`); `maxFramerate` wins when
    /// both are set (`TargetFramerate`).
    pub framerate: Option<f32>,
    /// The requested fixed output width (`width`).
    pub width: Option<i32>,
    /// The requested fixed output height (`height`).
    pub height: Option<i32>,
    /// The minimum segment count a live (`live.m3u8`) playlist waits for before
    /// it is served (`minSegments`); `None` derives it from the segment length
    /// (`StreamState.MinSegments`).
    pub min_segments: Option<i32>,
    /// The subtitle stream to deliver or burn in (`subtitleStreamIndex`).
    pub subtitle_stream_index: Option<i32>,
    /// The negotiated subtitle delivery method name (`subtitleMethod`:
    /// `Encode`/`Embed`/`External`/`Hls`/`Drop`).
    pub subtitle_method: Option<String>,
    /// The client's transcode reasons (`transcodeReasons`), forwarded into the
    /// master playlist's variant URL when the query lacks it.
    pub transcode_reasons: Option<String>,
    /// Whether text subtitle tracks are listed as a `#EXT-X-MEDIA` subtitle group
    /// in the master playlist (`enableSubtitlesInManifest`). The master route's
    /// DTO defaults this to `false` (`VideoRequestDto`); the live route defaults
    /// it to `true`.
    pub enable_subtitles_in_manifest: bool,
    /// Whether the master playlist adds two lower-bitrate variants
    /// (`enableAdaptiveBitrateStreaming`, default `false`). Ignored for
    /// requests from the local network.
    pub enable_adaptive_bitrate_streaming: bool,
    /// Whether the master playlist lists trickplay image playlists
    /// (`enableTrickplay`, default `true`).
    pub enable_trickplay: bool,
    /// The access token of the requesting session, embedded as `ApiKey` in the
    /// master playlist's subtitle/trickplay URIs (`ClaimsPrincipal.GetToken()`).
    pub api_key: Option<String>,
    /// Whether the request came from the local network
    /// (`INetworkManager.IsInLocalNetwork(remoteIp)`): disables the adaptive
    /// bitrate variants, which "within the local network will likely do more
    /// harm than good".
    pub is_in_local_network: bool,
    /// The raw request query string (including the leading `?`), forwarded into
    /// the generated playlist's segment URLs (`Request.QueryString`).
    pub query_string: String,
}

impl Default for HlsStreamRequest {
    fn default() -> Self {
        Self {
            item_id: Uuid::nil(),
            media_source_id: None,
            play_session_id: None,
            device_id: None,
            segment_container: None,
            segment_length: None,
            audio_codec: None,
            video_codec: None,
            transcoding_max_audio_channels: None,
            video_bitrate: None,
            audio_bitrate: None,
            max_width: None,
            max_height: None,
            max_framerate: None,
            // Stream copy is permitted unless the client explicitly forbade it
            // (the C# request DTO defaults both to true).
            allow_video_stream_copy: true,
            allow_audio_stream_copy: true,
            is_static: false,
            start_time_ticks: None,
            profile: None,
            level: None,
            framerate: None,
            width: None,
            height: None,
            min_segments: None,
            subtitle_stream_index: None,
            subtitle_method: None,
            transcode_reasons: None,
            // The master-playlist DTO defaults (`GetMasterHlsVideoPlaylist`):
            // no subtitle group, no adaptive variants, trickplay listed.
            enable_subtitles_in_manifest: false,
            enable_adaptive_bitrate_streaming: false,
            enable_trickplay: true,
            api_key: None,
            is_in_local_network: false,
            query_string: String::new(),
        }
    }
}

/// A resolved on-disk artifact to serve, with the MIME type to serve it as.
///
/// Port of the `(path, MimeTypes.GetMimeType(path))` pair the streaming
/// controllers hand to `FileStreamResponseHelpers.GetStaticFileResult`. The
/// caller (a `ferrofin-api` handler) streams the file at [`path`](Self::path) with
/// [`content_type`](Self::content_type); the transcode job that produced it is
/// kept alive / torn down by the [`HlsStreamManager`] internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedFile {
    /// The absolute path of the file to serve.
    pub path: String,
    /// The MIME type to serve it as (e.g. `video/mp2t`, `application/x-mpegURL`).
    pub content_type: String,
}

/// Serves the dynamic-HLS + transcode-stream flow behind one seam.
///
/// Port of the request→response essence of `DynamicHlsController` +
/// `HlsSegmentController` + the transcode branch of `VideosController` /
/// `UniversalAudioController`. Everything a handler needs — build a playlist
/// string, start-or-reuse a segment transcode and resolve the produced segment
/// file, resolve a legacy transcode-folder file, and run the progressive
/// transcode branch — lives here, so `ferrofin-api` handlers stay trait-only and
/// the `StreamState`/arg-building/ffmpeg-spawn machinery stays in the
/// implementation crate.
///
/// Implementations own the `TranscodeManager` job registry, the ffmpeg spawn
/// (via the segment-transcoder seam), and the `DynamicHlsPlaylistGenerator`; a
/// disabled implementation ([`crate::stubs::DisabledHlsStreamManager`]) returns
/// [`ServiceError::NotFound`] so hosts without a transcode runtime still route.
#[async_trait]
pub trait HlsStreamManager: Send + Sync {
    /// Builds the **master** HLS playlist (`master.m3u8`) for `request`.
    ///
    /// Port of `DynamicHlsController.GetMasterHls{Video,Audio}Playlist` →
    /// `DynamicHlsHelper.GetMasterHlsPlaylist`. `is_audio` selects the audio
    /// controller's master playlist.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the item/source cannot be resolved or the
    /// playlist cannot be produced.
    async fn master_playlist(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<String, ServiceError>;

    /// Builds the **variant** HLS playlist (`main.m3u8`) for `request`.
    ///
    /// Port of `GetVariantHls{Video,Audio}Playlist` → `GetVariantPlaylistInternal`
    /// → `DynamicHlsPlaylistGenerator.CreateMainPlaylist`.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the item/source cannot be resolved or the
    /// playlist cannot be produced.
    async fn variant_playlist(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<String, ServiceError>;

    /// Builds the **live** HLS playlist (`live.m3u8`) for `request`.
    ///
    /// Port of `DynamicHlsController.GetLiveHlsStream`.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the live stream cannot be resolved.
    async fn live_playlist(&self, request: &HlsStreamRequest) -> Result<String, ServiceError>;

    /// Starts (or reuses) the transcode for `request` and resolves segment
    /// `segment_id` on disk, ready to serve.
    ///
    /// Port of `GetHlsVideoSegment`/`GetHlsAudioSegment` → `GetDynamicSegment`
    /// (+ `StartFfMpeg` / `GetSegmentResult`). Waits until the segment exists or
    /// the transcode ends; a segment that never materialises is
    /// [`ServiceError::NotFound`].
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the transcode cannot be started or the
    /// segment does not appear.
    async fn dynamic_segment(
        &self,
        request: &HlsStreamRequest,
        segment_id: i32,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError>;

    /// Resolves a file inside the transcode cache directory by name, guarding
    /// against path traversal.
    ///
    /// Port of `HlsSegmentController.ValidateTranscodePath` + the
    /// `GetHlsVideoSegmentLegacy`/`GetHlsPlaylistLegacy`/`GetHlsAudioSegmentLegacy`
    /// serve path: `file_name` (`<segmentId><ext>`) must resolve *inside* the
    /// transcode folder. `require_m3u8` rejects a non-playlist match (the
    /// playlist-legacy route). Marks the owning job as begun so its keep-alive
    /// timer is refreshed. [`ServiceError::InvalidInput`] on a traversal/miss.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::InvalidInput`] when `file_name` escapes the
    /// transcode folder or does not resolve to an on-disk file.
    async fn resolve_transcode_file(
        &self,
        file_name: &str,
        require_m3u8: bool,
    ) -> Result<ServedFile, ServiceError>;

    /// Runs the **progressive transcode** branch of `/Videos/{id}/stream` (and
    /// the audio equivalent), returning the produced file to serve.
    ///
    /// Port of the transcoding branch of `VideosController.GetVideoStream` /
    /// `UniversalAudioController` (when the source is not direct-playable).
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the transcode cannot be produced.
    async fn transcode_stream(
        &self,
        request: &HlsStreamRequest,
        is_audio: bool,
    ) -> Result<ServedFile, ServiceError>;

    /// Stops the active encoding(s) for `request`'s device / play session,
    /// deleting their partial files.
    ///
    /// Port of `HlsSegmentController.StopEncodingProcess` → `KillTranscodingJobs`.
    /// Only [`device_id`](HlsStreamRequest::device_id) and
    /// [`play_session_id`](HlsStreamRequest::play_session_id) are read.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the kill cannot be dispatched.
    async fn stop_encoding(&self, request: &HlsStreamRequest) -> Result<(), ServiceError>;

    /// Refreshes the keep-alive timer for the transcoding job(s) of a play
    /// session so an active-but-paused transcode is not reaped.
    ///
    /// Port of `PlaystateController.PingPlaybackSession` →
    /// `TranscodeManager.PingTranscodingJob`. `is_user_paused`, when set, records
    /// whether the client paused playback. A ping for a session with no live job
    /// is a successful no-op (the client pings on a fixed interval regardless).
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::InvalidInput`] if `play_session_id` is empty.
    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError>;
}

/// Compile-time assertion that [`HlsStreamManager`] is object-safe.
fn _assert_object_safe_hls_stream_manager(_: &dyn HlsStreamManager) {}

#[cfg(test)]
mod tests {
    use super::{TranscodingJobType, TranscodingProgress};

    #[test]
    fn transcoding_progress_default_is_all_none() {
        let p = TranscodingProgress::default();
        assert!(p.position_ticks.is_none());
        assert!(p.framerate.is_none());
        assert!(p.percent_complete.is_none());
        assert!(p.bytes_transcoded.is_none());
        assert!(p.bit_rate.is_none());
    }

    #[test]
    fn transcoding_job_type_is_copy_and_eq() {
        let a = TranscodingJobType::Hls;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(TranscodingJobType::Progressive, TranscodingJobType::Dash);
    }
}
