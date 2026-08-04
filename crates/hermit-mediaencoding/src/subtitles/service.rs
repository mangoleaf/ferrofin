//! The `hermit-traits` [`SubtitleEncoder`] adapter over the pure encoder.
//!
//! Port of the service-layer surface of `MediaBrowser.MediaEncoding.Subtitles.
//! SubtitleEncoder` that the `SubtitleController` (and the streaming pipeline)
//! call: resolve the media source for `(item, mediaSourceId)`, locate the
//! subtitle stream by index, then delegate to the pure
//! [`SubtitleEncoder`](super::encoder::SubtitleEncoder) for the readable-file
//! resolution, charset normalization, and format conversion.
//!
//! The concrete encoder is generic over its [`SubtitleParser`] and
//! [`SubtitleIo`] seams; this adapter owns one and layers the item→media-source
//! lookup (the [`MediaSourceResolver`] seam reused from the attachment
//! extractor) on top so it satisfies the object-safe trait `AppState` names.

use std::sync::Arc;

use crate::error::MediaEncodingError;
use async_trait::async_trait;
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities::MediaStreamType;
use hermit_model::entities_media::MediaStream;
use hermit_traits::error::ServiceError;
use hermit_traits::media_encoding::SubtitleEncoder as SubtitleEncoderTrait;
use uuid::Uuid;

use super::encoder::{SubtitleEncoder, SubtitleIo};
use super::parser::SubtitleParser;
use crate::attachments::MediaSourceResolver;

/// The `hermit-traits` [`SubtitleEncoder`](SubtitleEncoderTrait) implementation.
///
/// Wraps the pure [`SubtitleEncoder`] with a [`MediaSourceResolver`] so the
/// trait's `item_id`/`media_source_id` arguments resolve to the concrete
/// [`MediaSourceInfo`] the pure logic operates on. Generic over the parser,
/// resolver, and I/O seams so tests inject fakes.
pub struct SubtitleEncoderImpl<P, R, I>
where
    P: SubtitleParser,
    R: MediaSourceResolver,
    I: SubtitleIo,
{
    encoder: Arc<SubtitleEncoder<P, I>>,
    resolver: Arc<R>,
}

impl<P, R, I> SubtitleEncoderImpl<P, R, I>
where
    P: SubtitleParser,
    R: MediaSourceResolver,
    I: SubtitleIo,
{
    /// Builds the adapter from a pure encoder and a media-source resolver.
    pub fn new(encoder: Arc<SubtitleEncoder<P, I>>, resolver: Arc<R>) -> Self {
        Self { encoder, resolver }
    }

    /// Resolves the media source for `(item_id, media_source_id)`, `404`-ing when
    /// it is absent. Port of the `GetPlaybackMediaSources(...).First(...)` lookup.
    async fn resolve_source(
        &self,
        item_id: Uuid,
        media_source_id: &str,
    ) -> Result<MediaSourceInfo, ServiceError> {
        self.resolver
            .resolve(item_id, media_source_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(format!("MediaSource {media_source_id} not found"))
            })
    }
}

/// Finds the subtitle stream at `index` within a media source.
///
/// Port of the `MediaStreams.First(i => i.Type == Subtitle && i.Index == index)`
/// selection; a missing index is a `404` rather than a panic.
fn find_subtitle_stream(
    media_source: &MediaSourceInfo,
    subtitle_stream_index: i32,
) -> Result<&MediaStream, ServiceError> {
    media_source
        .media_streams
        .iter()
        .find(|s| s.stream_type == MediaStreamType::Subtitle && s.index == subtitle_stream_index)
        .ok_or_else(|| {
            ServiceError::not_found(format!(
                "no subtitle stream with index {subtitle_stream_index}"
            ))
        })
}

#[async_trait]
impl<P, R, I> SubtitleEncoderTrait for SubtitleEncoderImpl<P, R, I>
where
    P: SubtitleParser,
    R: MediaSourceResolver,
    I: SubtitleIo,
{
    async fn get_subtitles(
        &self,
        item_id: Uuid,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_format: &str,
        start_time_ticks: i64,
        end_time_ticks: i64,
        preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, ServiceError> {
        if media_source_id.trim().is_empty() {
            return Err(ServiceError::invalid_input("mediaSourceId is empty"));
        }
        let media_source = self.resolve_source(item_id, media_source_id).await?;
        let subtitle_stream = find_subtitle_stream(&media_source, subtitle_stream_index)?;

        self.encoder
            .get_subtitles(
                &media_source,
                subtitle_stream,
                output_format,
                start_time_ticks,
                end_time_ticks,
                preserve_original_timestamps,
            )
            .await
            .map_err(|e| MediaEncodingError::process(e).into())
    }

    async fn get_subtitle_file_character_set(
        &self,
        subtitle_stream: &MediaStream,
        _language: &str,
        _media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError> {
        self.encoder
            .get_subtitle_file_character_set(subtitle_stream)
            .await
            .map_err(|e| MediaEncodingError::process(e).into())
    }

    async fn get_subtitle_file_path(
        &self,
        subtitle_stream: &MediaStream,
        media_source: &MediaSourceInfo,
    ) -> Result<String, ServiceError> {
        self.encoder
            .get_subtitle_file_path(media_source, subtitle_stream)
            .await
            .map_err(|e| MediaEncodingError::process(e).into())
    }

    async fn extract_all_extractable_subtitles(
        &self,
        media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        self.encoder
            .extract_all_extractable_subtitles(media_source)
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitles::encoder::{NoopSubtitleIo, SubtitleInfo, SubtitleIo};
    use crate::subtitles::parser::SubtitleEditParser;
    use hermit_model::media_info::MediaProtocol;

    /// A resolver returning a fixed (optional) media source.
    struct FixedResolver(Option<MediaSourceInfo>);

    #[async_trait]
    impl MediaSourceResolver for FixedResolver {
        async fn resolve(
            &self,
            _item_id: Uuid,
            _media_source_id: &str,
        ) -> Result<Option<MediaSourceInfo>, ServiceError> {
            Ok(self.0.clone())
        }
    }

    /// An I/O seam that serves in-memory bytes for any local read.
    #[derive(Clone)]
    struct MemIo {
        bytes: Vec<u8>,
    }

    #[async_trait]
    impl SubtitleIo for MemIo {
        async fn read_file(&self, _path: &str) -> Result<Vec<u8>, String> {
            Ok(self.bytes.clone())
        }
        async fn http_get(&self, _url: &str) -> Result<Vec<u8>, String> {
            Ok(self.bytes.clone())
        }
        fn path_protocol(&self, _path: &str) -> MediaProtocol {
            MediaProtocol::File
        }
        fn subtitle_cache_path(
            &self,
            _media_source_id: &str,
            _subtitle_stream_index: i32,
            _output_extension: &str,
        ) -> Option<String> {
            None
        }
        async fn extract(&self, _args: &str, _output_paths: &[String]) -> Result<(), String> {
            Ok(())
        }
    }

    /// An external SRT subtitle stream at `index` pointing at `path`.
    fn srt_stream(index: i32, path: &str) -> MediaStream {
        let mut s = MediaStream {
            stream_type: MediaStreamType::Subtitle,
            index,
            ..MediaStream::default()
        };
        s.is_external = true;
        s.codec = Some("srt".to_owned());
        s.path = Some(path.to_owned());
        s
    }

    fn source_with(streams: Vec<MediaStream>) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some("msrc".to_owned()),
            media_streams: streams,
            ..MediaSourceInfo::default()
        }
    }

    fn build(
        source: Option<MediaSourceInfo>,
        io: MemIo,
    ) -> SubtitleEncoderImpl<SubtitleEditParser, FixedResolver, MemIo> {
        let encoder = Arc::new(SubtitleEncoder::new(SubtitleEditParser, io));
        SubtitleEncoderImpl::new(encoder, Arc::new(FixedResolver(source)))
    }

    const SRT_SAMPLE: &str = "1\n00:00:01,000 --> 00:00:02,000\nHello\n";

    #[tokio::test]
    async fn empty_media_source_id_is_invalid_input() {
        let enc = build(None, MemIo { bytes: Vec::new() });
        let err = enc
            .get_subtitles(Uuid::nil(), "  ", 0, "vtt", 0, 0, false)
            .await
            .expect_err("empty id rejected");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_media_source_is_not_found() {
        let enc = build(None, MemIo { bytes: Vec::new() });
        let err = enc
            .get_subtitles(Uuid::nil(), "msrc", 0, "vtt", 0, 0, false)
            .await
            .expect_err("missing source");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn missing_subtitle_index_is_not_found() {
        let source = source_with(vec![srt_stream(0, "/subs/a.srt")]);
        let enc = build(Some(source), MemIo { bytes: Vec::new() });
        let err = enc
            .get_subtitles(Uuid::nil(), "msrc", 7, "vtt", 0, 0, false)
            .await
            .expect_err("missing index");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn converts_srt_to_vtt() {
        let source = source_with(vec![srt_stream(0, "/subs/a.srt")]);
        let io = MemIo {
            bytes: SRT_SAMPLE.as_bytes().to_vec(),
        };
        let enc = build(Some(source), io);
        let out = enc
            .get_subtitles(Uuid::nil(), "msrc", 0, "vtt", 0, 0, false)
            .await
            .expect("convert");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.starts_with("WEBVTT"), "expected WEBVTT header: {text}");
        assert!(text.contains("Hello"));
    }

    #[tokio::test]
    async fn same_format_returns_original_bytes() {
        let source = source_with(vec![srt_stream(0, "/subs/a.srt")]);
        let io = MemIo {
            bytes: SRT_SAMPLE.as_bytes().to_vec(),
        };
        let enc = build(Some(source), io);
        let out = enc
            .get_subtitles(Uuid::nil(), "msrc", 0, "srt", 0, 0, false)
            .await
            .expect("passthrough");
        assert_eq!(out, SRT_SAMPLE.as_bytes());
    }

    #[tokio::test]
    async fn character_set_of_utf8_srt_is_empty() {
        let io = MemIo {
            bytes: SRT_SAMPLE.as_bytes().to_vec(),
        };
        let enc = build(None, io);
        let stream = srt_stream(0, "/subs/a.srt");
        let charset = enc
            .get_subtitle_file_character_set(&stream, "eng", &MediaSourceInfo::default())
            .await
            .expect("charset");
        assert_eq!(charset, "");
    }

    #[tokio::test]
    async fn file_path_of_external_stream_is_its_path() {
        let source = source_with(vec![srt_stream(0, "/subs/a.srt")]);
        let io = MemIo { bytes: Vec::new() };
        let enc = build(Some(source.clone()), io);
        let stream = srt_stream(0, "/subs/a.srt");
        let path = enc
            .get_subtitle_file_path(&stream, &source)
            .await
            .expect("path");
        assert_eq!(path, "/subs/a.srt");
    }

    #[tokio::test]
    async fn extract_all_is_ok_noop_when_nothing_extractable() {
        let source = source_with(vec![srt_stream(0, "/subs/a.srt")]);
        let enc = build(Some(source.clone()), MemIo { bytes: Vec::new() });
        enc.extract_all_extractable_subtitles(&source)
            .await
            .expect("noop ok");
    }

    #[tokio::test]
    async fn readable_info_default_is_file_protocol() {
        // Exercise the SubtitleInfo default used across the encoder surface.
        let info = SubtitleInfo::default();
        assert_eq!(info.protocol, MediaProtocol::File);
        assert!(!info.is_external);
        // NoopSubtitleIo is the default seam; confirm its cache path is None.
        let noop = NoopSubtitleIo;
        assert!(noop.subtitle_cache_path("x", 0, ".srt").is_none());
    }
}
