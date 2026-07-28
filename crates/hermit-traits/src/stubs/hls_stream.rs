//! A disabled [`HlsStreamManager`] for hosts without a transcode runtime.
//!
//! The [`HlsStreamManager`] seam belongs to the (real) `hermit-mediaencoding`
//! transcode runtime, not a deferred subsystem — but `AppState` must name a
//! non-optional `Arc<dyn HlsStreamManager>` and every existing test constructor
//! predates the seam. This stub lets those callers (and any host that ships
//! without ffmpeg) satisfy the field: every method reports the flow as
//! unavailable ([`ServiceError::NotFound`]), which the streaming handlers map to
//! their `404`/`501`-shaped responses. The composition root replaces it with the
//! concrete `HlsStreamManagerImpl` when a transcode runtime is present.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::media_encoding::{
    AttachmentExtractor, ExtractedAttachment, HlsStreamManager, HlsStreamRequest, ServedFile,
    SubtitleEncoder,
};
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities_media::MediaStream;

/// A no-op [`HlsStreamManager`]: every flow reports "no transcode runtime".
///
/// Returned by hosts without ffmpeg and used as the `AppState` default so
/// pre-seam test constructors keep compiling. Never transcodes anything.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledHlsStreamManager;

impl DisabledHlsStreamManager {
    /// The uniform "transcoding is unavailable on this host" error.
    fn unavailable() -> ServiceError {
        ServiceError::NotFound("transcoding is not available on this server".to_owned())
    }
}

#[async_trait]
impl HlsStreamManager for DisabledHlsStreamManager {
    async fn master_playlist(
        &self,
        _request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<String, ServiceError> {
        Err(Self::unavailable())
    }

    async fn variant_playlist(
        &self,
        _request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<String, ServiceError> {
        Err(Self::unavailable())
    }

    async fn live_playlist(&self, _request: &HlsStreamRequest) -> Result<String, ServiceError> {
        Err(Self::unavailable())
    }

    async fn dynamic_segment(
        &self,
        _request: &HlsStreamRequest,
        _segment_id: i32,
        _is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        Err(Self::unavailable())
    }

    async fn resolve_transcode_file(
        &self,
        _file_name: &str,
        _require_m3u8: bool,
    ) -> Result<ServedFile, ServiceError> {
        Err(Self::unavailable())
    }

    async fn transcode_stream(
        &self,
        _request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<ServedFile, ServiceError> {
        Err(Self::unavailable())
    }

    async fn stop_encoding(&self, _request: &HlsStreamRequest) -> Result<(), ServiceError> {
        // Nothing is running on a disabled host; stopping is a successful no-op
        // so the client's `DELETE` still returns `204`.
        Ok(())
    }

    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        _is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError> {
        // No jobs run on a disabled host, but the empty-id contract still holds so
        // the handler's `204` semantics match the concrete manager.
        if play_session_id.trim().is_empty() {
            return Err(ServiceError::invalid_input("playSessionId is empty"));
        }
        Ok(())
    }
}

/// A no-op [`AttachmentExtractor`]: every request reports "no transcode runtime".
///
/// The `Videos/{id}/{source}/Attachments/{index}` handler needs an
/// [`AttachmentExtractor`] in `AppState`; this default lets pre-seam test
/// constructors keep compiling and reports a missing attachment as
/// [`ServiceError::NotFound`]. The composition root replaces it with the
/// concrete ffmpeg-backed extractor.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledAttachmentExtractor;

#[async_trait]
impl AttachmentExtractor for DisabledAttachmentExtractor {
    async fn get_attachment(
        &self,
        _item_id: Uuid,
        _media_source_id: &str,
        _attachment_stream_index: i32,
    ) -> Result<ExtractedAttachment, ServiceError> {
        Err(ServiceError::NotFound(
            "attachment extraction is not available on this server".to_owned(),
        ))
    }

    async fn extract_all_attachments(
        &self,
        _input_file: &str,
        _media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::NotFound(
            "attachment extraction is not available on this server".to_owned(),
        ))
    }
}

/// A no-op [`SubtitleEncoder`]: every request reports "no transcode runtime".
///
/// The on-the-fly subtitle-conversion routes
/// (`Videos/{id}/{source}/Subtitles/{index}/{format}` and the HLS subtitle
/// playlist) need a [`SubtitleEncoder`] in `AppState`; this default lets
/// pre-seam test constructors keep compiling and reports a subtitle it cannot
/// produce as [`ServiceError::NotFound`]. The composition root replaces it with
/// the concrete ffmpeg-backed encoder.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledSubtitleEncoder;

impl DisabledSubtitleEncoder {
    /// The uniform "subtitle encoding is unavailable on this host" error.
    fn unavailable() -> ServiceError {
        ServiceError::NotFound("subtitle conversion is not available on this server".to_owned())
    }
}

#[async_trait]
impl SubtitleEncoder for DisabledSubtitleEncoder {
    async fn get_subtitles(
        &self,
        _item_id: Uuid,
        _media_source_id: &str,
        _subtitle_stream_index: i32,
        _output_format: &str,
        _start_time_ticks: i64,
        _end_time_ticks: i64,
        _preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, ServiceError> {
        Err(Self::unavailable())
    }

    async fn get_subtitle_file_character_set(
        &self,
        _subtitle_stream: &MediaStream,
        _language: &str,
        _media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError> {
        Err(Self::unavailable())
    }

    async fn get_subtitle_file_path(
        &self,
        _subtitle_stream: &MediaStream,
        _media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError> {
        Err(Self::unavailable())
    }

    async fn extract_all_extractable_subtitles(
        &self,
        _media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        Err(Self::unavailable())
    }
}
