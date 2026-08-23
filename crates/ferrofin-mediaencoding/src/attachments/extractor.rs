//! Port of `AttachmentExtractor`.

use std::sync::Arc;

use crate::error::MediaEncodingError;
use crate::keyed_locks::KeyedLocks;
use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaAttachment;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{AttachmentExtractor, ExtractedAttachment, MediaEncoder};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

/// Resolves an item's playback media sources.
///
/// Port of the `IMediaSourceManager.GetPlaybackMediaSources` call inside
/// `GetAttachment`; kept behind a seam because the media-source manager is not
/// part of this crate. Returns the resolved [`MediaSourceInfo`] for
/// `(item_id, media_source_id)`.
#[async_trait]
pub trait MediaSourceResolver: Send + Sync {
    /// Returns the media source with `media_source_id` for `item_id`, if any.
    async fn resolve(
        &self,
        item_id: Uuid,
        media_source_id: &str,
    ) -> Result<Option<MediaSourceInfo>, ServiceError>;
}

/// The un-mockable attachment I/O: the ffmpeg `-dump_attachment` spawn plus the
/// cache-folder/file operations.
///
/// Port of the `Process`/`File`/`Directory` calls in the C# extractor; behind a
/// seam so tests inject a fake and no real process/filesystem work runs.
#[async_trait]
pub trait AttachmentIo: Send + Sync {
    /// Returns the cache folder for `media_source_id`'s attachments, or `None`
    /// when the id is not a cache-eligible GUID.
    ///
    /// Port of `IPathManager.GetAttachmentFolderPath`.
    fn attachment_folder_path(&self, media_source_id: &str) -> Option<String>;

    /// Returns the on-disk path for the named attachment file within
    /// `media_source_id`'s cache folder.
    ///
    /// Port of `IPathManager.GetAttachmentPath`.
    fn attachment_path(&self, media_source_id: &str, file_name: &str) -> String;

    /// Returns whether `path` names an existing file.
    fn file_exists(&self, path: &str) -> bool;

    /// Creates `path` (and its parents) if absent — C# `Directory.CreateDirectory`,
    /// which the extractor runs before ffmpeg writes into the cache folder.
    ///
    /// # Errors
    ///
    /// Returns an error string when the directory cannot be created.
    async fn create_directory(&self, path: &str) -> Result<(), String>;

    /// Reads the bytes of `path`.
    ///
    /// # Errors
    ///
    /// Returns an error string when the file cannot be read.
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;

    /// Runs ffmpeg with `args` (using the resolved `ffmpeg_path`) to extract
    /// attachments, returning the process exit code. `working_dir` is the
    /// process's current directory when set — the batch dump
    /// (`-dump_attachment:t ""`) writes each file relative to it, which is why
    /// C# sets `WorkingDirectory = outputFolder`.
    ///
    /// # Errors
    ///
    /// Returns an error string when the process cannot be spawned.
    async fn run_ffmpeg(
        &self,
        ffmpeg_path: &str,
        args: &str,
        working_dir: Option<&str>,
    ) -> Result<i32, String>;
}

/// A no-op [`AttachmentIo`] for tests that never touch the disk or ffmpeg.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAttachmentIo;

#[async_trait]
impl AttachmentIo for NoopAttachmentIo {
    fn attachment_folder_path(&self, _media_source_id: &str) -> Option<String> {
        None
    }

    fn attachment_path(&self, _media_source_id: &str, file_name: &str) -> String {
        file_name.to_owned()
    }

    fn file_exists(&self, _path: &str) -> bool {
        false
    }

    async fn create_directory(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    async fn read_file(&self, _path: &str) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    async fn run_ffmpeg(
        &self,
        _ffmpeg_path: &str,
        _args: &str,
        _working_dir: Option<&str>,
    ) -> Result<i32, String> {
        Ok(0)
    }
}

/// The `ferrofin-traits` [`AttachmentExtractor`] implementation.
///
/// Generic over the [`MediaEncoder`] (for input-argument building), the
/// [`MediaSourceResolver`] (item→source lookup) and the [`AttachmentIo`] seam
/// (ffmpeg + filesystem).
pub struct AttachmentExtractorImpl<E, R, I>
where
    E: MediaEncoder,
    R: MediaSourceResolver,
    I: AttachmentIo,
{
    media_encoder: Arc<E>,
    resolver: Arc<R>,
    io: Arc<I>,
    /// Per-output-folder locks (port of `AsyncKeyedLocker<string>`), serializing
    /// extraction so two callers never race on the same cache folder.
    locks: KeyedLocks,
}

impl<E, R, I> AttachmentExtractorImpl<E, R, I>
where
    E: MediaEncoder,
    R: MediaSourceResolver,
    I: AttachmentIo,
{
    /// Creates an extractor from the encoder, resolver, and I/O seams.
    pub fn new(media_encoder: Arc<E>, resolver: Arc<R>, io: Arc<I>) -> Self {
        Self {
            media_encoder,
            resolver,
            io,
            locks: KeyedLocks::new(),
        }
    }

    /// Returns the keyed lock for `key`, creating it on first use.
    fn lock_for(&self, key: &str) -> Arc<AsyncMutex<()>> {
        self.locks.get(key)
    }

    /// Whether the source has any video or audio stream (drives the dummy
    /// `-t 0 -f null null` output in the C# arg builders).
    fn has_video_or_audio(media_source: &MediaSourceInfo) -> bool {
        media_source.media_streams.iter().any(|s| {
            s.stream_type == MediaStreamType::Video || s.stream_type == MediaStreamType::Audio
        })
    }

    /// Extracts one attachment to its cache path if absent, returning the path.
    ///
    /// Port of `ExtractAttachment` + `ExtractAttachmentInternal`.
    async fn extract_attachment(
        &self,
        input_file: &str,
        media_source: &MediaSourceInfo,
        attachment: &MediaAttachment,
    ) -> Result<String, ServiceError> {
        let source_id = media_source.id.as_deref().unwrap_or_default();
        let folder = self
            .io
            .attachment_folder_path(source_id)
            .ok_or_else(|| {
                ServiceError::not_found(format!(
                    "MediaSource {source_id} has no attachment cache (non-GUID Id, e.g. Live TV stream)."
                ))
            })?;

        let lock = self.lock_for(&folder);
        let _guard = lock.lock().await;

        let index_name = attachment.index.to_string();
        let file_name = attachment.file_name.as_deref().unwrap_or(&index_name);
        let attachment_path = self.io.attachment_path(source_id, file_name);

        if !self.io.file_exists(&attachment_path) {
            let input_path = self
                .media_encoder
                .get_input_argument(input_file, media_source);
            if input_path.is_empty() {
                return Err(ServiceError::invalid_input("empty input path"));
            }
            // `Directory.CreateDirectory(Path.GetDirectoryName(outputPath))`.
            self.io
                .create_directory(&folder)
                .await
                .map_err(MediaEncodingError::process)?;
            let has_av = Self::has_video_or_audio(media_source);
            let tail = if has_av { "-t 0 -f null null" } else { "" };
            let args = format!(
                "-dump_attachment:{} \"{}\" -i {} {}",
                attachment.index,
                normalize_path(&attachment_path),
                input_path,
                tail
            )
            .trim()
            .to_owned();

            let exit_code = self
                .io
                .run_ffmpeg(&self.media_encoder.encoder_path(), &args, None)
                .await
                .map_err(MediaEncodingError::process)?;

            // Exit code 1 with no A/V stream is the expected/harmless
            // "no output" case (see the C# comment).
            let failed = (exit_code != 0 && (has_av || exit_code != 1))
                || !self.io.file_exists(&attachment_path);
            if failed {
                return Err(ServiceError::backend(format!(
                    "ffmpeg attachment extraction failed for {input_path} to {attachment_path}"
                )));
            }
        }

        Ok(attachment_path)
    }
}

#[async_trait]
impl<E, R, I> AttachmentExtractor for AttachmentExtractorImpl<E, R, I>
where
    E: MediaEncoder,
    R: MediaSourceResolver,
    I: AttachmentIo,
{
    async fn get_attachment(
        &self,
        item_id: Uuid,
        media_source_id: &str,
        attachment_stream_index: i32,
    ) -> Result<ExtractedAttachment, ServiceError> {
        if media_source_id.trim().is_empty() {
            return Err(ServiceError::invalid_input("mediaSourceId is empty"));
        }

        let media_source = self
            .resolver
            .resolve(item_id, media_source_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(format!("MediaSource {media_source_id} not found"))
            })?;

        let attachment = media_source
            .media_attachments
            .iter()
            .find(|a| a.index == attachment_stream_index)
            .cloned()
            .ok_or_else(|| {
                ServiceError::not_found(format!(
                    "MediaSource {media_source_id} has no attachment with stream index {attachment_stream_index}"
                ))
            })?;

        if attachment
            .codec
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("mjpeg"))
        {
            return Err(ServiceError::not_found(format!(
                "Attachment with stream index {attachment_stream_index} can't be extracted for MediaSource {media_source_id}"
            )));
        }

        let input_file = media_source.path.clone().unwrap_or_default();
        let attachment_path = self
            .extract_attachment(&input_file, &media_source, &attachment)
            .await?;
        let data = self
            .io
            .read_file(&attachment_path)
            .await
            .map_err(MediaEncodingError::process)?;

        Ok(ExtractedAttachment { attachment, data })
    }

    async fn extract_all_attachments(
        &self,
        input_file: &str,
        media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        let source_id = media_source.id.as_deref().unwrap_or_default();
        // C# logs-and-returns when the id is not a GUID.
        let Some(folder) = self.io.attachment_folder_path(source_id) else {
            return Ok(());
        };

        let lock = self.lock_for(&folder);
        let _guard = lock.lock().await;

        // Skip extraction when every extractable attachment file already exists
        // (C#: only attachments WITH a file name count — the batch dump names its
        // output after the `filename` tag, so a nameless one can never be found).
        let missing: Vec<&MediaAttachment> = media_source
            .media_attachments
            .iter()
            .filter(|a| {
                !a.codec
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case("mjpeg"))
            })
            .filter(|a| a.file_name.is_some())
            .filter(|a| {
                let index_name = a.index.to_string();
                let file_name = a.file_name.as_deref().unwrap_or(&index_name);
                let path = self.io.attachment_path(source_id, file_name);
                !self.io.file_exists(&path)
            })
            .collect();

        if missing.is_empty() {
            return Ok(());
        }

        let input_path = self
            .media_encoder
            .get_input_argument(input_file, media_source);
        if input_path.is_empty() {
            return Err(ServiceError::invalid_input("empty input path"));
        }

        let concat = if input_path.ends_with(".concat\"") {
            "-f concat -safe 0"
        } else {
            ""
        };
        // `Directory.CreateDirectory(outputFolder)`, then ffmpeg runs INSIDE it
        // (`WorkingDirectory = outputFolder`): the batch dump writes each
        // attachment to its own `filename` relative to the current directory.
        self.io
            .create_directory(&folder)
            .await
            .map_err(MediaEncodingError::process)?;
        let has_av = Self::has_video_or_audio(media_source);
        let tail = if has_av { "-t 0 -f null null" } else { "" };
        let args = format!("-dump_attachment:t \"\" -y {concat} -i {input_path} {tail}")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let exit_code = self
            .io
            .run_ffmpeg(&self.media_encoder.encoder_path(), &args, Some(&folder))
            .await
            .map_err(MediaEncodingError::process)?;

        if exit_code != 0 && (has_av || exit_code != 1) {
            return Err(ServiceError::backend(format!(
                "ffmpeg attachment extraction failed for {input_path} to {folder}"
            )));
        }

        Ok(())
    }
}

/// Escapes embedded double quotes with a leading backslash.
///
/// Port of `EncodingUtils.NormalizePath`, duplicated locally so the extractor
/// does not reach into a sibling module's private helper.
fn normalize_path(path: &str) -> String {
    path.replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities_media::MediaAttachment;
    use ferrofin_model::media_info::MediaProtocol;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_encoding::{AttachmentExtractor, MediaEncoder, MediaInfoRequest};
    use uuid::Uuid;

    use super::{AttachmentExtractorImpl, AttachmentIo, MediaSourceResolver};

    /// A minimal [`MediaEncoder`] fake exposing only the two methods the
    /// extractor calls.
    struct FakeEncoder;

    #[async_trait]
    impl MediaEncoder for FakeEncoder {
        fn encoder_path(&self) -> String {
            "/usr/bin/ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "/usr/bin/ffprobe".to_owned()
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
            Ok(String::new())
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &ferrofin_model::entities_media::MediaStream,
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            format!("file:\"{input_file}\"")
        }
        fn get_time_parameter(&self, _ticks: i64) -> String {
            String::new()
        }
        async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

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

    /// An [`AttachmentIo`] fake recording the args it was asked to run and
    /// pretending the output appears after extraction.
    struct FakeIo {
        folder: Option<String>,
        existing: bool,
        exit_code: i32,
        last_args: Mutex<Option<String>>,
        created_dir: Mutex<Option<String>>,
        working_dir: Mutex<Option<String>>,
    }

    #[async_trait]
    impl AttachmentIo for FakeIo {
        fn attachment_folder_path(&self, _media_source_id: &str) -> Option<String> {
            self.folder.clone()
        }
        fn attachment_path(&self, _media_source_id: &str, file_name: &str) -> String {
            format!("/cache/{file_name}")
        }
        fn file_exists(&self, _path: &str) -> bool {
            // After a "successful" run we report the file as present so the
            // failure check passes.
            self.existing || self.last_args.lock().unwrap().is_some()
        }
        async fn create_directory(&self, path: &str) -> Result<(), String> {
            *self.created_dir.lock().unwrap() = Some(path.to_owned());
            Ok(())
        }
        async fn read_file(&self, _path: &str) -> Result<Vec<u8>, String> {
            Ok(b"FONT".to_vec())
        }
        async fn run_ffmpeg(
            &self,
            _ffmpeg_path: &str,
            args: &str,
            working_dir: Option<&str>,
        ) -> Result<i32, String> {
            *self.last_args.lock().unwrap() = Some(args.to_owned());
            *self.working_dir.lock().unwrap() = working_dir.map(str::to_owned);
            Ok(self.exit_code)
        }
    }

    fn source_with_attachment() -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some("0f2c8d1e-1111-2222-3333-444455556666".to_owned()),
            path: Some("/media/movie.mkv".to_owned()),
            protocol: MediaProtocol::File,
            media_attachments: vec![MediaAttachment {
                index: 3,
                codec: Some("ttf".to_owned()),
                file_name: Some("font.ttf".to_owned()),
                ..MediaAttachment::default()
            }],
            ..MediaSourceInfo::default()
        }
    }

    fn extractor(
        source: Option<MediaSourceInfo>,
        io: FakeIo,
    ) -> AttachmentExtractorImpl<FakeEncoder, FixedResolver, FakeIo> {
        AttachmentExtractorImpl::new(
            Arc::new(FakeEncoder),
            Arc::new(FixedResolver(source)),
            Arc::new(io),
        )
    }

    #[tokio::test]
    async fn get_attachment_extracts_and_returns_bytes() {
        let io = FakeIo {
            folder: Some("/cache".to_owned()),
            existing: false,
            exit_code: 0,
            last_args: Mutex::new(None),
            created_dir: Mutex::new(None),
            working_dir: Mutex::new(None),
        };
        let ex = extractor(Some(source_with_attachment()), io);
        let out = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap();
        assert_eq!(out.data, b"FONT");
        assert_eq!(out.attachment.index, 3);
        // The cache folder is created before ffmpeg writes into it
        // (`Directory.CreateDirectory`); the single dump names its full output
        // path, so no working directory is needed.
        assert_eq!(ex.io.created_dir.lock().unwrap().as_deref(), Some("/cache"));
        assert_eq!(ex.io.working_dir.lock().unwrap().as_deref(), None);
    }

    #[tokio::test]
    async fn get_attachment_rejects_mjpeg() {
        let mut source = source_with_attachment();
        source.media_attachments[0].codec = Some("mjpeg".to_owned());
        let io = FakeIo {
            folder: Some("/cache".to_owned()),
            existing: false,
            exit_code: 0,
            last_args: Mutex::new(None),
            created_dir: Mutex::new(None),
            working_dir: Mutex::new(None),
        };
        let ex = extractor(Some(source), io);
        let err = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_attachment_missing_index_is_not_found() {
        let io = FakeIo {
            folder: Some("/cache".to_owned()),
            existing: false,
            exit_code: 0,
            last_args: Mutex::new(None),
            created_dir: Mutex::new(None),
            working_dir: Mutex::new(None),
        };
        let ex = extractor(Some(source_with_attachment()), io);
        let err = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 99)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_attachment_empty_source_id_is_invalid() {
        let io = FakeIo {
            folder: Some("/cache".to_owned()),
            existing: false,
            exit_code: 0,
            last_args: Mutex::new(None),
            created_dir: Mutex::new(None),
            working_dir: Mutex::new(None),
        };
        let ex = extractor(Some(source_with_attachment()), io);
        let err = ex.get_attachment(Uuid::nil(), "   ", 3).await.unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn extract_all_skips_when_folder_is_not_a_guid() {
        let io = FakeIo {
            folder: None,
            existing: false,
            exit_code: 0,
            last_args: Mutex::new(None),
            created_dir: Mutex::new(None),
            working_dir: Mutex::new(None),
        };
        let source = source_with_attachment();
        let ex = extractor(Some(source.clone()), io);
        assert!(
            ex.extract_all_attachments("/media/movie.mkv", &source)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn extract_all_runs_ffmpeg_for_missing_files() {
        let io = FakeIo {
            folder: Some("/cache".to_owned()),
            existing: false,
            exit_code: 0,
            last_args: Mutex::new(None),
            created_dir: Mutex::new(None),
            working_dir: Mutex::new(None),
        };
        let source = source_with_attachment();
        let ex = extractor(Some(source.clone()), io);
        ex.extract_all_attachments("/media/movie.mkv", &source)
            .await
            .unwrap();
        // Extraction dumped to the cache with the batch flag, from INSIDE the
        // (freshly created) cache folder: the batch dump writes cwd-relative.
        let ex_io = &ex.io;
        assert!(
            ex_io
                .last_args
                .lock()
                .unwrap()
                .as_deref()
                .unwrap()
                .contains("-dump_attachment:t")
        );
        assert_eq!(ex_io.created_dir.lock().unwrap().as_deref(), Some("/cache"));
        assert_eq!(ex_io.working_dir.lock().unwrap().as_deref(), Some("/cache"));
    }
}
