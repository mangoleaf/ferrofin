//! Media-encoding composition — assembles the ffmpeg-backed transcode/HLS pair
//! and the attachment extractor that the composition root injects via
//! [`AppState::with_media_encoding`](hermit_api::AppState::with_media_encoding).
//!
//! Port of the Autofac registration of `ITranscodeManager` +
//! `IDynamicHlsPlaylistGenerator` (→ the HLS stream manager) and
//! `IAttachmentExtractor` in `Jellyfin.Server`'s `Startup`. Everything below the
//! [`StreamStatePlanner`](hermit_hls::StreamStatePlanner) seam
//! (`TokioSegmentTranscoder` → `TranscodeManagerImpl` → `HlsStreamManagerImpl`)
//! is already ported in `hermit-mediaencoding`/`hermit-hls`; this unit builds the
//! concrete [`HermitStreamStatePlanner`](crate::planner::HermitStreamStatePlanner)
//! that feeds it and the two small server-local adapters the attachment extractor
//! needs (a [`MediaSourceResolver`] over the [`MediaSourceManager`] and a real
//! ffmpeg/filesystem [`AttachmentIo`]).

use std::sync::Arc;

use async_trait::async_trait;
use hermit_core::HermitServerApplicationPaths;
use hermit_hls::{DynamicHlsPlaylistGenerator, HlsStreamManagerImpl};
use hermit_mediaencoding::attachments::{AttachmentIo, MediaSourceResolver};
use hermit_mediaencoding::{
    AttachmentExtractorImpl, EncodingHelper, NoOptionalEncoders, NoopSessionReporter,
    TokioSegmentTranscoder, TranscodeManagerImpl,
};
use hermit_model::configuration::EncodingOptions;
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities::Video3DFormat;
use hermit_model::entities_media::MediaStream;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::error::ServiceError;
use hermit_traits::library::MediaSourceManager;
use hermit_traits::media_encoding::{
    AttachmentExtractor, HlsStreamManager, MediaEncoder, MediaInfoRequest,
};
use hermit_traits::system::PathManager;
use uuid::Uuid;

use crate::planner::HermitStreamStatePlanner;

/// Builds the concrete media-encoding pair injected via
/// [`AppState::with_media_encoding`](hermit_api::AppState::with_media_encoding).
///
/// Returns `(hls, attachments)`:
/// - `hls` — the [`HlsStreamManagerImpl`] wiring the
///   [`HermitStreamStatePlanner`](crate::planner::HermitStreamStatePlanner) →
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
#[must_use]
pub fn build_media_encoding(
    media_sources: Arc<dyn MediaSourceManager>,
    encoder: Arc<dyn MediaEncoder>,
    config: Arc<dyn ServerConfigurationManager>,
    paths: Arc<HermitServerApplicationPaths>,
    path_manager: Arc<dyn PathManager>,
) -> (Arc<dyn HlsStreamManager>, Arc<dyn AttachmentExtractor>) {
    // ---- HLS transcode chain (below the planner seam) ---------------------
    let planner = HermitStreamStatePlanner::new(
        Arc::clone(&media_sources),
        Arc::clone(&encoder),
        EncodingHelper::new(NoOptionalEncoders),
        config,
        Arc::clone(&paths),
    );
    let transcoder = TokioSegmentTranscoder::new();
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    // The generator reads live encoding options per request; First-Light returns
    // the defaults (the persisted named-config accessor is not yet threaded
    // through `ServerConfigurationManager`).
    let generator_config: Box<dyn Fn() -> EncodingOptions + Send + Sync> =
        Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(
        generator_config,
        Vec::new(),
    ));
    let hls: Arc<dyn HlsStreamManager> = Arc::new(HlsStreamManagerImpl::new(
        planner,
        transcoder,
        manager,
        generator,
        paths as Arc<dyn hermit_traits::system::ServerApplicationPaths>,
    ));

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

    (hls, attachments)
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
/// [`MediaSourceInfo`] with its attachments populated (the static source list
/// carries streams but not attachments, so the attachment rows are fetched and
/// merged here).
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
        let Some(mut source) = sources
            .into_iter()
            .find(|s| s.id.as_deref() == Some(media_source_id))
        else {
            return Ok(None);
        };
        // The static source carries streams but not attachments; fill them from
        // the attachment repository so `get_attachment` can locate the row.
        source.media_attachments = self.media_sources.get_media_attachments(item_id).await?;
        Ok(Some(source))
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
        self.path_manager
            .attachment_path(media_source_id, file_name)
            .unwrap_or_else(|| file_name.to_owned())
    }

    fn file_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        tokio::fs::read(path)
            .await
            .map_err(|e| format!("read {path}: {e}"))
    }

    async fn run_ffmpeg(&self, ffmpeg_path: &str, args: &str) -> Result<i32, String> {
        // `args` is a pre-built ffmpeg command line (a single space-joined
        // string, matching the C# `arguments` string); split it into parts while
        // honouring the double-quoted attachment-output path.
        let parts = split_ffmpeg_args(args);
        let status = tokio::process::Command::new(ffmpeg_path)
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
    use hermit_model::entities::MediaStreamType;
    use hermit_model::entities_media::MediaAttachment;
    use hermit_model::media_info::LiveStreamRequest;

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
        Arc<HermitServerApplicationPaths>,
    ) {
        let paths = Arc::new(HermitServerApplicationPaths::new(
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
    struct FakeConfig(Arc<HermitServerApplicationPaths>);

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        fn application_paths(&self) -> Arc<dyn hermit_traits::system::ServerApplicationPaths> {
            Arc::clone(&self.0) as Arc<_>
        }
        async fn configuration(
            &self,
        ) -> Result<hermit_model::configuration::ServerConfiguration, ServiceError> {
            Ok(hermit_model::configuration::ServerConfiguration::default())
        }
        async fn update_configuration(
            &self,
            _configuration: &hermit_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<hermit_model::branding::BrandingOptions, ServiceError> {
            Ok(hermit_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &hermit_model::branding::BrandingOptions,
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
        // The pair builds without panicking; both slots are real trait objects.
        let (_hls, _attachments) =
            build_media_encoding(media_sources, encoder, config, paths, path_manager);
    }

    #[tokio::test]
    async fn resolver_finds_source_and_merges_attachments() {
        let attachment = MediaAttachment {
            index: 1,
            file_name: Some("font.ttf".to_owned()),
            ..MediaAttachment::default()
        };
        let resolver = MediaSourceManagerResolver {
            media_sources: Arc::new(FakeSources {
                sources: vec![source("abc")],
                attachments: vec![attachment],
            }),
        };
        let outcome = resolver
            .resolve(Uuid::from_u128(1), "abc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("abc"));
        // The attachment rows are merged in from get_media_attachments.
        assert_eq!(outcome.media_attachments.len(), 1);
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
