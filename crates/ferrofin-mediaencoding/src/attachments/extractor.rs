//! Port of `AttachmentExtractor`.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::encoder::quote;
use crate::error::MediaEncodingError;
use crate::keyed_locks::KeyedLocks;
use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaAttachment;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::OpenedFile;
use ferrofin_traits::media_encoding::{
    AttachmentExtractor, ExtractedAttachment, MediaEncoder, MediaInfoRequest,
};
use ferrofin_util::source_url::redact_urls_in;
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
    /// `media_source_id`'s cache folder, or `None` when the id has no cache
    /// folder or the name has no file-name leaf (`..`, a trailing separator).
    ///
    /// Port of `IPathManager.GetAttachmentPath`: only the name's leaf is kept,
    /// so the path never leaves the cache folder.
    fn attachment_path(&self, media_source_id: &str, file_name: &str) -> Option<String>;

    /// Returns whether `path` names an existing file.
    fn file_exists(&self, path: &str) -> bool;

    /// Deletes the file at `path` — `IFileSystem.DeleteFile`, which a failed
    /// extraction runs on each output it may have left half-written.
    ///
    /// # Errors
    ///
    /// Returns an error string when the file exists but cannot be deleted.
    async fn remove_file(&self, path: &str) -> Result<(), String>;

    /// Creates `path` (and its parents) if absent — C# `Directory.CreateDirectory`,
    /// which the extractor runs before ffmpeg writes into the cache folder.
    ///
    /// # Errors
    ///
    /// Returns an error string when the directory cannot be created.
    async fn create_directory(&self, path: &str) -> Result<(), String>;

    /// Opens `path` to stream it — `AsyncFile.OpenRead`.
    ///
    /// # Errors
    ///
    /// The I/O error, its kind intact: a missing file is the `404` upstream's
    /// `FileNotFoundException` is.
    async fn open_file(&self, path: &str) -> std::io::Result<OpenedFile>;

    /// Runs ffmpeg with `args` (using the resolved `ffmpeg_path`) to extract
    /// attachments, returning the process exit code. `working_dir` is the
    /// process's current directory when set — the batch dump
    /// (`-dump_attachment:t ""`) writes each file relative to it, which is why
    /// C# sets `WorkingDirectory = outputFolder`.
    ///
    /// `outputs` are the files the run writes, where they are known before it
    /// starts. A run whose future is dropped before ffmpeg exits kills ffmpeg
    /// and deletes them, as upstream's cancellation branch would: `Kill(true)`,
    /// then the exit code -1 takes the failure path, which deletes what the run
    /// may have half-written. The extractor runs every extraction on a task of
    /// its own, so that happens only when the host tears down (a shutdown or a
    /// restart, [`AttachmentExtractor::shutdown`]), never when a caller stops
    /// waiting.
    ///
    /// # Errors
    ///
    /// Returns an error string when `args` does not split into argv
    /// (unbalanced quoting) or the process cannot be spawned.
    async fn run_ffmpeg(
        &self,
        ffmpeg_path: &str,
        args: &str,
        working_dir: Option<&str>,
        outputs: &[String],
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

    fn attachment_path(&self, _media_source_id: &str, file_name: &str) -> Option<String> {
        Some(file_name.to_owned())
    }

    fn file_exists(&self, _path: &str) -> bool {
        false
    }

    async fn remove_file(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    async fn create_directory(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    async fn open_file(&self, path: &str) -> std::io::Result<OpenedFile> {
        Ok(OpenedFile {
            path: path.to_owned(),
            length: 0,
            reader: Box::new(std::io::empty()),
        })
    }

    async fn run_ffmpeg(
        &self,
        _ffmpeg_path: &str,
        _args: &str,
        _working_dir: Option<&str>,
        _outputs: &[String],
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
    locks: Arc<KeyedLocks>,
    /// Set by [`AttachmentExtractor::shutdown`]; each running extraction holds
    /// a receiver until it is gone.
    stop: Arc<tokio::sync::watch::Sender<bool>>,
}

/// A handle on the same extractor: it shares the seams and the folder locks,
/// so an extraction finishing on a task of its own still holds the lock other
/// callers wait on.
impl<E, R, I> Clone for AttachmentExtractorImpl<E, R, I>
where
    E: MediaEncoder,
    R: MediaSourceResolver,
    I: AttachmentIo,
{
    fn clone(&self) -> Self {
        Self {
            media_encoder: Arc::clone(&self.media_encoder),
            resolver: Arc::clone(&self.resolver),
            io: Arc::clone(&self.io),
            locks: Arc::clone(&self.locks),
            stop: Arc::clone(&self.stop),
        }
    }
}

/// The error an extraction stopped by [`AttachmentExtractor::shutdown`] ends in.
fn stopped() -> ServiceError {
    ServiceError::backend("attachment extraction stopped: the server is shutting down")
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
            locks: Arc::default(),
            stop: Arc::new(tokio::sync::watch::Sender::new(false)),
        }
    }

    /// Runs an extraction on a task of its own and waits for it.
    ///
    /// Upstream extracts with a token nothing cancels: the attachment route
    /// passes `CancellationToken.None`, and a transcode's token is its own,
    /// made before the job that could cancel it exists. So a caller that stops
    /// waiting (a client hanging up) does not stop the extraction: ffmpeg runs
    /// to its end under the folder's lock, and the next request is served from
    /// the cache. Only [`AttachmentExtractor::shutdown`] stops it.
    async fn run_to_completion<T: Send + 'static>(
        &self,
        extraction: impl Future<Output = Result<T, ServiceError>> + Send + 'static,
    ) -> Result<T, ServiceError> {
        let mut stop = self.stop.subscribe();
        if *stop.borrow_and_update() {
            return Err(stopped());
        }
        // Under the caller's span, which awaits it, as the item delete's task is:
        // a slow source keeps that span open until ffmpeg ends.
        tokio::spawn(tracing::Instrument::in_current_span(async move {
            tokio::select! {
                result = extraction => result,
                // Dropping the extraction kills its ffmpeg and deletes what it
                // was writing.
                Ok(_) = stop.wait_for(|is_stopped| *is_stopped) => Err(stopped()),
            }
        }))
        .await
        .map_err(|error| {
            ServiceError::backend(format!("attachment extraction task failed: {error}"))
        })?
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

    /// The cache path `attachment` is extracted to: `GetAttachmentPath(id,
    /// FileName ?? index) ?? GetAttachmentPath(id, index)`. A name with no
    /// leaf falls back to the attachment's index.
    fn cache_path(
        &self,
        source_id: &str,
        attachment: &MediaAttachment,
    ) -> Result<String, ServiceError> {
        let index_name = attachment.index.to_string();
        let file_name = attachment.file_name.as_deref().unwrap_or(&index_name);
        self.io
            .attachment_path(source_id, file_name)
            .or_else(|| self.io.attachment_path(source_id, &index_name))
            .ok_or_else(|| {
                ServiceError::not_found(format!(
                    "MediaSource {source_id} has no attachment cache (non-GUID Id, e.g. Live TV stream)."
                ))
            })
    }

    /// Whether any attachment's file name is more than a safe leaf (a path,
    /// `..`), so a batch dump would write it outside the cache folder.
    /// `ExtractAllAttachments`' `shouldExtractOneByOne` test.
    fn has_unsafe_file_name(attachments: &[MediaAttachment]) -> bool {
        attachments.iter().any(|a| {
            a.file_name.as_deref().is_some_and(|name| {
                !name.is_empty()
                    && ferrofin_util::path_helper::get_safe_leaf_file_name(Some(name)).as_deref()
                        != Some(name)
            })
        })
    }

    /// `-f concat -safe 0` for a DVD/Blu-ray concat input, which upstream
    /// recognises by its `.concat"` ending, ignoring case.
    fn concat_arguments(input_path: &str) -> &'static str {
        let ending = ".concat\"";
        let is_concat = input_path.len() >= ending.len()
            && input_path.as_bytes()[input_path.len() - ending.len()..]
                .eq_ignore_ascii_case(ending.as_bytes());
        if is_concat { "-f concat -safe 0" } else { "" }
    }

    /// Dumps each missing attachment of `attachments` to its own cache path,
    /// in one ffmpeg run. Port of `ExtractAllAttachmentsIndividuallyInternal`.
    ///
    /// Every output path is chosen here ([`Self::cache_path`]), so a name
    /// from the media file never decides where ffmpeg writes, and ffmpeg runs
    /// with no working directory. A failed run deletes what it may have left
    /// half-written, and a run that did not write every file is a failure.
    async fn extract_individually(
        &self,
        input_file: &str,
        media_source: &MediaSourceInfo,
        attachments: &[MediaAttachment],
        has_av: bool,
        folder: &str,
    ) -> Result<(), ServiceError> {
        let source_id = media_source.id.as_deref().unwrap_or_default();
        let input_path = self
            .media_encoder
            .get_input_argument(input_file, media_source);
        if input_path.is_empty() {
            return Err(ServiceError::invalid_input("empty input path"));
        }
        self.io
            .create_directory(folder)
            .await
            .map_err(MediaEncodingError::process)?;

        let mut dump = String::new();
        let mut missing = Vec::new();
        for attachment in attachments.iter().filter(|a| {
            !a.codec
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("mjpeg"))
        }) {
            let path = self.cache_path(source_id, attachment)?;
            if self.io.file_exists(&path) {
                continue;
            }
            let _ = write!(
                dump,
                "-dump_attachment:{} {} ",
                attachment.index,
                quote(&path)
            );
            missing.push(path);
        }
        if missing.is_empty() {
            return Ok(());
        }

        let concat = Self::concat_arguments(&input_path);
        let tail = if has_av { "-t 0 -f null null" } else { "" };
        let args = format!("{dump}{concat} -i {input_path} {tail}")
            .trim()
            .to_owned();
        let exit_code = self
            .io
            .run_ffmpeg(&self.media_encoder.encoder_path(), &args, None, &missing)
            .await
            .map_err(MediaEncodingError::process)?;

        let mut failed = exit_code != 0 && (has_av || exit_code != 1);
        if failed {
            for path in missing.iter().filter(|path| self.io.file_exists(path)) {
                if let Err(error) = self.io.remove_file(path).await {
                    tracing::error!(%error, path, "deleting an extracted attachment failed");
                }
            }
        }
        failed = failed || missing.iter().any(|path| !self.io.file_exists(path));
        if failed {
            return Err(ServiceError::backend(format!(
                "ffmpeg attachment extraction failed for {} to {folder}",
                redact_urls_in(&input_path)
            )));
        }
        Ok(())
    }

    /// Dumps every attachment by the name its `filename` tag gives it, from
    /// inside the cache folder. Port of `ExtractAllAttachmentsInternal`, run
    /// only for inputs whose names are all safe leaves.
    async fn extract_batch(
        &self,
        input_path: &str,
        has_av: bool,
        folder: &str,
    ) -> Result<(), ServiceError> {
        let concat = Self::concat_arguments(input_path);
        // `Directory.CreateDirectory(outputFolder)`, then ffmpeg runs INSIDE it
        // (`WorkingDirectory = outputFolder`): the batch dump writes each
        // attachment to its own `filename` relative to the current directory.
        self.io
            .create_directory(folder)
            .await
            .map_err(MediaEncodingError::process)?;
        let tail = if has_av { "-t 0 -f null null" } else { "" };
        // Trim only: collapsing whitespace would also collapse it inside the
        // quoted input path.
        let args = format!("-dump_attachment:t \"\" -y {concat} -i {input_path} {tail}")
            .trim()
            .to_owned();

        let exit_code = self
            .io
            // Its files are named by the media's own `filename` tags, so none
            // is known to delete on a cancelled run; upstream's failure path
            // only removes the folder when it is empty (`Directory.Delete`).
            .run_ffmpeg(&self.media_encoder.encoder_path(), &args, Some(folder), &[])
            .await
            .map_err(MediaEncodingError::process)?;

        if exit_code != 0 && (has_av || exit_code != 1) {
            return Err(ServiceError::backend(format!(
                "ffmpeg attachment extraction failed for {} to {folder}",
                redact_urls_in(input_path)
            )));
        }
        Ok(())
    }

    /// The attachments of an external `.mks` sidecar, as its own probe lists
    /// them.
    ///
    /// The media source's attachments describe the main file, not the
    /// sidecar, so they say nothing about the names a batch dump of the
    /// sidecar would write.
    async fn probe_sidecar(
        &self,
        input_file: &str,
        media_source: &MediaSourceInfo,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                id: media_source.id.clone(),
                path: Some(input_file.to_owned()),
                protocol: MediaProtocol::File,
                ..MediaSourceInfo::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        self.media_encoder.get_media_info(&request).await
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

        let attachment_path = self.cache_path(source_id, attachment)?;

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
                "-dump_attachment:{} {} -i {} {}",
                attachment.index,
                quote(&attachment_path),
                input_path,
                tail
            )
            .trim()
            .to_owned();

            let exit_code = self
                .io
                .run_ffmpeg(
                    &self.media_encoder.encoder_path(),
                    &args,
                    None,
                    std::slice::from_ref(&attachment_path),
                )
                .await
                .map_err(MediaEncodingError::process)?;

            // Exit code 1 with no A/V stream is the expected/harmless
            // "no output" case (see the C# comment). A failed run deletes
            // what it may have half-written, which would otherwise be served
            // as the attachment from then on.
            let mut failed = exit_code != 0 && (has_av || exit_code != 1);
            if failed
                && self.io.file_exists(&attachment_path)
                && let Err(error) = self.io.remove_file(&attachment_path).await
            {
                tracing::error!(%error, path = attachment_path, "deleting an extracted attachment failed");
            }
            failed = failed || !self.io.file_exists(&attachment_path);
            if failed {
                return Err(ServiceError::backend(format!(
                    "ffmpeg attachment extraction failed for {} to {attachment_path}",
                    redact_urls_in(&input_path)
                )));
            }
        }

        Ok(attachment_path)
    }

    /// Extracts every attachment of `media_source` to its cache folder, under
    /// the folder's lock. Port of `ExtractAllAttachments`.
    async fn extract_all(
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

        // `ExtractAllAttachments`: a name that is not a safe leaf would make
        // the batch dump write outside the cache folder, so every attachment
        // gets a path of its own instead. An `.mks` sidecar is excepted
        // upstream, because the source's attachments describe the main file.
        let is_mks = input_file.len() >= 4
            && input_file.as_bytes()[input_file.len() - 4..].eq_ignore_ascii_case(b".mks");
        if !is_mks && Self::has_unsafe_file_name(&media_source.media_attachments) {
            return self
                .extract_individually(
                    input_file,
                    media_source,
                    &media_source.media_attachments,
                    Self::has_video_or_audio(media_source),
                    &folder,
                )
                .await;
        }

        // Skip extraction when every extractable attachment file already exists
        // (C#: only attachments WITH a file name count — the batch dump names its
        // output after the `filename` tag, so a nameless one can never be found).
        let any_missing = media_source
            .media_attachments
            .iter()
            .filter(|a| {
                !a.codec
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case("mjpeg"))
            })
            .filter_map(|a| a.file_name.as_deref())
            .any(|name| {
                self.io
                    .attachment_path(source_id, name)
                    .is_none_or(|path| !self.io.file_exists(&path))
            });
        if !any_missing {
            return Ok(());
        }

        // Divergence, a Jellyfin bug not ported: upstream batch-dumps an
        // `.mks` whatever its names, so there a sidecar whose own `filename`
        // tags are paths writes outside the cache folder. The sidecar's own
        // probe names its attachments here, and an unsafe one sends them
        // down the per-attachment path, by the sidecar's own indexes. The
        // check above is upstream's and reads the main file's names, so while
        // those stay missing every call probes again (and, with safe names,
        // re-runs the dump, as upstream does).
        if is_mks {
            let sidecar = self.probe_sidecar(input_file, media_source).await?;
            if Self::has_unsafe_file_name(&sidecar.media_attachments) {
                return self
                    .extract_individually(
                        input_file,
                        media_source,
                        &sidecar.media_attachments,
                        Self::has_video_or_audio(&sidecar),
                        &folder,
                    )
                    .await;
            }
        }

        let input_path = self
            .media_encoder
            .get_input_argument(input_file, media_source);
        if input_path.is_empty() {
            return Err(ServiceError::invalid_input("empty input path"));
        }
        self.extract_batch(&input_path, Self::has_video_or_audio(media_source), &folder)
            .await
    }
}

#[async_trait]
impl<E, R, I> AttachmentExtractor for AttachmentExtractorImpl<E, R, I>
where
    E: MediaEncoder + 'static,
    R: MediaSourceResolver + 'static,
    I: AttachmentIo + 'static,
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

        let (extractor, extracting) = (self.clone(), attachment.clone());
        let attachment_path = self
            .run_to_completion(async move {
                let input_file = media_source.path.clone().unwrap_or_default();
                extractor
                    .extract_attachment(&input_file, &media_source, &extracting)
                    .await
            })
            .await?;
        // `GetAttachmentStream`'s `AsyncFile.OpenRead`: a file gone since the
        // extraction throws `FileNotFoundException` or
        // `DirectoryNotFoundException` (`ENOENT`, `ENOTDIR`), which upstream's
        // exception middleware answers `404`. The message names no path: it
        // reaches the client, and the route takes anonymous callers.
        let file = self.io.open_file(&attachment_path).await.map_err(|error| {
            match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    tracing::debug!(path = attachment_path, %error, "attachment file not found");
                    ServiceError::not_found("attachment file not found")
                }
                _ => MediaEncodingError::io(format!("open {attachment_path}"), error).into(),
            }
        })?;

        Ok(ExtractedAttachment { attachment, file })
    }

    async fn extract_all_attachments(
        &self,
        input_file: &str,
        media_source: &MediaSourceInfo,
    ) -> Result<(), ServiceError> {
        let (extractor, input_file, media_source) =
            (self.clone(), input_file.to_owned(), media_source.clone());
        self.run_to_completion(
            async move { extractor.extract_all(&input_file, &media_source).await },
        )
        .await
    }

    async fn shutdown(&self) {
        self.stop.send_replace(true);
        // Each running extraction drops its receiver once its ffmpeg is killed
        // and its outputs are deleted.
        self.stop.closed().await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Read as _;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities_media::MediaAttachment;
    use ferrofin_model::media_info::MediaProtocol;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_encoding::{AttachmentExtractor, MediaEncoder, MediaInfoRequest};
    use tokio::sync::Notify;
    use uuid::Uuid;

    use super::{AttachmentExtractorImpl, AttachmentIo, MediaSourceResolver};

    /// A minimal [`MediaEncoder`] fake exposing only the methods the
    /// extractor calls. Its probe answers with `sidecar`'s attachments and
    /// records the path it was asked about.
    #[derive(Default)]
    struct FakeEncoder {
        sidecar: Vec<MediaAttachment>,
        probed: Mutex<Vec<String>>,
    }

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
            request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            self.probed
                .lock()
                .unwrap()
                .push(request.media_source.path.clone().unwrap_or_default());
            Ok(MediaSourceInfo {
                media_attachments: self.sidecar.clone(),
                ..MediaSourceInfo::default()
            })
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
            crate::encoder::get_input_argument(
                "file",
                input_file,
                ferrofin_model::media_info::MediaProtocol::File,
            )
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

    /// An [`AttachmentIo`] fake over an in-memory set of files, recording the
    /// args it was asked to run. A run writes every output it names
    /// (`-dump_attachment:N <path>`) unless `writes` is off, whatever its
    /// exit code, the way a failing ffmpeg can leave a partial file behind.
    /// Opening a file answers `FONT`, or fails with `open_error`.
    struct FakeIo {
        folder: Option<String>,
        exit_code: i32,
        writes: bool,
        open_error: Option<std::io::ErrorKind>,
        files: Mutex<HashSet<String>>,
        removed: Mutex<Vec<String>>,
        last_args: Mutex<Option<String>>,
        created_dir: Mutex<Option<String>>,
        working_dir: Mutex<Option<String>>,
        outputs: Mutex<Vec<String>>,
        /// When set, the first run signals the first and waits for the
        /// second before it does anything, like an ffmpeg still reading a
        /// slow source.
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
        /// How many runs started.
        runs: AtomicUsize,
    }

    impl FakeIo {
        fn new(folder: Option<&str>, exit_code: i32) -> Self {
            Self {
                folder: folder.map(str::to_owned),
                exit_code,
                writes: true,
                open_error: None,
                files: Mutex::new(HashSet::new()),
                removed: Mutex::new(Vec::new()),
                last_args: Mutex::new(None),
                created_dir: Mutex::new(None),
                working_dir: Mutex::new(None),
                outputs: Mutex::new(Vec::new()),
                gate: None,
                runs: AtomicUsize::new(0),
            }
        }

        fn with_files(self, files: &[&str]) -> Self {
            self.files
                .lock()
                .unwrap()
                .extend(files.iter().map(|f| (*f).to_owned()));
            self
        }

        fn args(&self) -> Option<String> {
            self.last_args.lock().unwrap().clone()
        }

        /// The outputs the last run named for deletion if it were cancelled.
        fn outputs(&self) -> Vec<String> {
            self.outputs.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AttachmentIo for FakeIo {
        fn attachment_folder_path(&self, _media_source_id: &str) -> Option<String> {
            self.folder.clone()
        }
        fn attachment_path(&self, _media_source_id: &str, file_name: &str) -> Option<String> {
            // `PathManager.GetAttachmentPath`: the name's leaf, or nothing.
            ferrofin_util::path_helper::get_safe_leaf_file_name(Some(file_name))
                .map(|leaf| format!("/cache/{leaf}"))
        }
        fn file_exists(&self, path: &str) -> bool {
            self.files.lock().unwrap().contains(path)
        }
        async fn remove_file(&self, path: &str) -> Result<(), String> {
            self.files.lock().unwrap().remove(path);
            self.removed.lock().unwrap().push(path.to_owned());
            Ok(())
        }
        async fn create_directory(&self, path: &str) -> Result<(), String> {
            *self.created_dir.lock().unwrap() = Some(path.to_owned());
            Ok(())
        }
        async fn open_file(&self, path: &str) -> std::io::Result<super::OpenedFile> {
            if let Some(kind) = self.open_error {
                return Err(kind.into());
            }
            Ok(super::OpenedFile {
                path: path.to_owned(),
                length: 4,
                reader: Box::new(&b"FONT"[..]),
            })
        }
        async fn run_ffmpeg(
            &self,
            _ffmpeg_path: &str,
            args: &str,
            working_dir: Option<&str>,
            outputs: &[String],
        ) -> Result<i32, String> {
            let first = self.runs.fetch_add(1, Ordering::SeqCst) == 0;
            if first && let Some((started, release)) = &self.gate {
                started.notify_one();
                release.notified().await;
            }
            *self.last_args.lock().unwrap() = Some(args.to_owned());
            outputs.clone_into(&mut self.outputs.lock().unwrap());
            *self.working_dir.lock().unwrap() = working_dir.map(str::to_owned);
            if self.writes {
                let tokens = crate::encoder::split_args(args)?;
                for pair in tokens.windows(2) {
                    if pair[0].starts_with("-dump_attachment:") && !pair[1].is_empty() {
                        self.files.lock().unwrap().insert(pair[1].clone());
                    }
                }
            }
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
        extractor_with(FakeEncoder::default(), source, io)
    }

    fn extractor_with(
        encoder: FakeEncoder,
        source: Option<MediaSourceInfo>,
        io: FakeIo,
    ) -> AttachmentExtractorImpl<FakeEncoder, FixedResolver, FakeIo> {
        AttachmentExtractorImpl::new(
            Arc::new(encoder),
            Arc::new(FixedResolver(source)),
            Arc::new(io),
        )
    }

    fn attachment(index: i32, file_name: Option<&str>) -> MediaAttachment {
        MediaAttachment {
            index,
            codec: Some("ttf".to_owned()),
            file_name: file_name.map(str::to_owned),
            ..MediaAttachment::default()
        }
    }

    fn source_with(attachments: Vec<MediaAttachment>) -> MediaSourceInfo {
        MediaSourceInfo {
            media_attachments: attachments,
            ..source_with_attachment()
        }
    }

    #[tokio::test]
    async fn get_attachment_extracts_and_returns_the_opened_file() {
        let io = FakeIo::new(Some("/cache"), 0);
        let ex = extractor(Some(source_with_attachment()), io);
        let mut out = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap();
        let mut data = Vec::new();
        out.file.reader.read_to_end(&mut data).unwrap();
        assert_eq!(
            (out.file.path.as_str(), out.file.length),
            ("/cache/font.ttf", 4)
        );
        assert_eq!(data, b"FONT");
        assert_eq!(out.attachment.index, 3);
        // The cache folder is created before ffmpeg writes into it
        // (`Directory.CreateDirectory`); the single dump names its full output
        // path, so no working directory is needed.
        assert_eq!(ex.io.created_dir.lock().unwrap().as_deref(), Some("/cache"));
        assert_eq!(ex.io.working_dir.lock().unwrap().as_deref(), None);
        // A cancelled run deletes the file it was writing.
        assert_eq!(ex.io.outputs(), ["/cache/font.ttf"]);
    }

    /// A gated extractor of one attachment whose name is not a leaf: the
    /// transcode path dumps it by index, to the cache path the attachment
    /// route uses.
    fn gated_extractor() -> (
        AttachmentExtractorImpl<FakeEncoder, FixedResolver, FakeIo>,
        MediaSourceInfo,
        Arc<Notify>,
        Arc<Notify>,
    ) {
        let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let mut io = FakeIo::new(Some("/cache"), 0);
        io.gate = Some((Arc::clone(&started), Arc::clone(&release)));
        let source = source_with(vec![attachment(3, Some("fonts/font.ttf"))]);
        (
            extractor(Some(source.clone()), io),
            source,
            started,
            release,
        )
    }

    /// One call of either caller: the attachment route or the transcode path.
    async fn extract(
        ex: &AttachmentExtractorImpl<FakeEncoder, FixedResolver, FakeIo>,
        source: &MediaSourceInfo,
        all: bool,
    ) -> Result<(), ServiceError> {
        if all {
            ex.extract_all_attachments("/media/movie.mkv", source).await
        } else {
            ex.get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
                .await
                .map(drop)
        }
    }

    /// A caller that stops waiting (a client hanging up mid-dump) does not
    /// stop the extraction. Upstream's attachment route passes
    /// `CancellationToken.None`, and a transcode's token is its own, made
    /// before the job that could cancel it, so ffmpeg runs to its end, the
    /// file stays, and the next request is served from the cache.
    #[rstest::rstest]
    #[case::attachment_route(false)]
    #[case::transcode(true)]
    #[tokio::test]
    async fn a_dropped_caller_does_not_stop_the_extraction(#[case] all: bool) {
        let (ex, source, started, release) = gated_extractor();

        // The caller is dropped while ffmpeg runs.
        tokio::select! {
            _ = extract(&ex, &source, all) => panic!("the extraction ended before ffmpeg was let go"),
            () = started.notified() => {}
        }
        release.notify_one();

        // The next caller waits for the extraction, which still holds the
        // folder, then finds its file: no second ffmpeg.
        extract(&ex, &source, all).await.unwrap();
        assert!(ex.io.file_exists("/cache/font.ttf"));
        assert!(ex.io.removed.lock().unwrap().is_empty());
        assert_eq!(ex.io.runs.load(Ordering::SeqCst), 1);
    }

    /// The host's teardown (a shutdown or a restart) stops an extraction
    /// nobody waits for any more, so no ffmpeg outlives the server lifetime
    /// that started it, and refuses new ones.
    #[rstest::rstest]
    #[case::attachment_route(false)]
    #[case::transcode(true)]
    #[tokio::test]
    async fn shutdown_stops_a_running_extraction(#[case] all: bool) {
        let (ex, source, started, release) = gated_extractor();
        tokio::select! {
            _ = extract(&ex, &source, all) => panic!("the extraction ended before ffmpeg was let go"),
            () = started.notified() => {}
        }

        // Returns once the run is gone: dropped, so ffmpeg is killed, and the
        // folder's lock is free.
        ex.shutdown().await;
        assert!(ex.lock_for("/cache").try_lock().is_ok());
        release.notify_one();
        tokio::task::yield_now().await;

        assert!(!ex.io.file_exists("/cache/font.ttf"));
        assert!(matches!(
            extract(&ex, &source, all).await,
            Err(ServiceError::Backend(message)) if message.contains("shutting down")
        ));
        assert_eq!(ex.io.runs.load(Ordering::SeqCst), 1);
    }

    /// A cached file gone before it is opened is upstream's
    /// `FileNotFoundException`/`DirectoryNotFoundException`, a `404`; any other
    /// failure to open it is a `500`.
    #[rstest::rstest]
    #[case(std::io::ErrorKind::NotFound, true)]
    #[case(std::io::ErrorKind::NotADirectory, true)]
    #[case(std::io::ErrorKind::PermissionDenied, false)]
    #[tokio::test]
    async fn get_attachment_maps_a_failed_open(
        #[case] kind: std::io::ErrorKind,
        #[case] not_found: bool,
    ) {
        let mut io = FakeIo::new(Some("/cache"), 0);
        io.open_error = Some(kind);
        let error = extractor(Some(source_with_attachment()), io)
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap_err();
        if not_found {
            assert!(
                matches!(&error, ServiceError::NotFound(m) if !m.contains("/cache")),
                "{error:?}"
            );
        } else {
            assert!(matches!(error, ServiceError::BackendSource(_)), "{error:?}");
        }
    }

    #[tokio::test]
    async fn get_attachment_rejects_mjpeg() {
        let mut source = source_with_attachment();
        source.media_attachments[0].codec = Some("mjpeg".to_owned());
        let io = FakeIo::new(Some("/cache"), 0);
        let ex = extractor(Some(source), io);
        let err = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_attachment_missing_index_is_not_found() {
        let io = FakeIo::new(Some("/cache"), 0);
        let ex = extractor(Some(source_with_attachment()), io);
        let err = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 99)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_attachment_empty_source_id_is_invalid() {
        let io = FakeIo::new(Some("/cache"), 0);
        let ex = extractor(Some(source_with_attachment()), io);
        let err = ex.get_attachment(Uuid::nil(), "   ", 3).await.unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn extract_all_skips_when_folder_is_not_a_guid() {
        let io = FakeIo::new(None, 0);
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
        let io = FakeIo::new(Some("/cache"), 0);
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
        // The media names the batch dump's files, so a cancelled run has none
        // it could delete.
        let outputs = ex_io.outputs();
        assert!(outputs.is_empty(), "{outputs:?}");
    }

    /// The batch dump used to collapse every run of whitespace in the whole
    /// command line, including inside the quoted input path. The path must
    /// reach ffmpeg byte for byte, quotes and double spaces included.
    #[tokio::test]
    async fn extract_all_keeps_the_input_path_intact() {
        let io = FakeIo::new(Some("/cache"), 0);
        let source = source_with_attachment();
        let ex = extractor(Some(source.clone()), io);
        let path = r#"/media/The  "Best" \ Movie.mkv"#;
        ex.extract_all_attachments(path, &source).await.unwrap();
        let args = ex.io.last_args.lock().unwrap().clone().unwrap();
        let tokens = crate::encoder::split_args(&args).unwrap();
        let i = tokens.iter().position(|a| a == "-i").unwrap();
        assert_eq!(tokens[i + 1], format!("file:{path}"));
        assert_eq!(tokens[..3], ["-dump_attachment:t", "", "-y"]);
    }

    /// `ExtractAllAttachments`: a `filename` tag that is more than a safe
    /// leaf would make the batch dump write wherever the tag points, so every
    /// attachment is dumped by its index to a path inside the cache folder,
    /// with no working directory and no `-y`.
    #[rstest::rstest]
    #[case::traversal("../../evil.ttf", "/cache/evil.ttf")]
    #[case::absolute("/abs/x.ttf", "/cache/x.ttf")]
    #[case::subfolder("fonts/Arial.ttf", "/cache/Arial.ttf")]
    #[case::no_leaf("..", "/cache/3")]
    #[tokio::test]
    async fn an_unsafe_name_dumps_each_attachment_to_its_own_cache_path(
        #[case] name: &str,
        #[case] path: &str,
    ) {
        let source = source_with(vec![
            attachment(3, Some(name)),
            attachment(4, Some("safe.otf")),
            attachment(5, None),
        ]);
        let ex = extractor(Some(source.clone()), FakeIo::new(Some("/cache"), 0));
        ex.extract_all_attachments("/media/movie.mkv", &source)
            .await
            .unwrap();
        let args = ex.io.args().unwrap();
        let tokens = crate::encoder::split_args(&args).unwrap();
        assert_eq!(
            tokens,
            [
                "-dump_attachment:3",
                path,
                "-dump_attachment:4",
                "/cache/safe.otf",
                "-dump_attachment:5",
                "/cache/5",
                "-i",
                "file:/media/movie.mkv",
            ]
        );
        assert_eq!(ex.io.working_dir.lock().unwrap().as_deref(), None);
        assert_eq!(ex.io.created_dir.lock().unwrap().as_deref(), Some("/cache"));
    }

    /// The per-attachment dump skips what is already extracted, and runs no
    /// ffmpeg at all once everything is.
    #[tokio::test]
    async fn the_per_attachment_dump_skips_extracted_files() {
        let source = source_with(vec![
            attachment(3, Some("../a.ttf")),
            attachment(4, Some("b.ttf")),
        ]);
        let io = FakeIo::new(Some("/cache"), 0).with_files(&["/cache/a.ttf"]);
        let ex = extractor(Some(source.clone()), io);
        ex.extract_all_attachments("/media/movie.mkv", &source)
            .await
            .unwrap();
        let args = ex.io.args().unwrap();
        assert!(args.starts_with("-dump_attachment:4 "), "{args}");
        assert!(!args.contains("-dump_attachment:3"), "{args}");
        // Only the file this run writes is deleted if it is cancelled, never
        // the one already extracted.
        assert_eq!(ex.io.outputs(), ["/cache/b.ttf"]);

        *ex.io.last_args.lock().unwrap() = None;
        ex.extract_all_attachments("/media/movie.mkv", &source)
            .await
            .unwrap();
        assert_eq!(ex.io.args(), None);
    }

    /// A failed run deletes the outputs it may have left half-written, and a
    /// run that exits cleanly without writing them all is a failure too.
    #[tokio::test]
    async fn a_failed_per_attachment_dump_deletes_its_outputs() {
        let mut source = source_with(vec![attachment(3, Some("../a.ttf"))]);
        source
            .media_streams
            .push(ferrofin_model::entities_media::MediaStream {
                stream_type: ferrofin_model::entities::MediaStreamType::Video,
                ..ferrofin_model::entities_media::MediaStream::default()
            });
        let ex = extractor(Some(source.clone()), FakeIo::new(Some("/cache"), 1));
        let error = ex
            .extract_all_attachments("/media/movie.mkv", &source)
            .await
            .unwrap_err();
        assert!(matches!(error, ServiceError::Backend(_)), "{error:?}");
        assert_eq!(*ex.io.removed.lock().unwrap(), ["/cache/a.ttf"]);
        assert!(ex.io.args().unwrap().ends_with("-t 0 -f null null"));

        let mut io = FakeIo::new(Some("/cache"), 0);
        io.writes = false;
        let ex = extractor(Some(source.clone()), io);
        assert!(
            ex.extract_all_attachments("/media/movie.mkv", &source)
                .await
                .is_err()
        );
        assert!(ex.io.removed.lock().unwrap().is_empty());
    }

    /// Without an audio or video stream ffmpeg has nothing to write to its
    /// dummy output and exits 1 after the dump, which is success.
    #[tokio::test]
    async fn exit_code_one_without_audio_or_video_is_success() {
        let source = source_with(vec![attachment(3, Some("../a.ttf"))]);
        let ex = extractor(Some(source.clone()), FakeIo::new(Some("/cache"), 1));
        ex.extract_all_attachments("/media/movie.mkv", &source)
            .await
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(ex.io.removed.lock().unwrap().is_empty());
        assert!(!ex.io.args().unwrap().contains("null"));
    }

    /// An `.mks` sidecar is batch-dumped upstream whatever the main file's
    /// names, because those describe the main file. Its own probe decides
    /// here: unsafe names go down the per-attachment path by the sidecar's
    /// own indexes, safe ones keep the batch dump.
    #[rstest::rstest]
    #[case::unsafe_sidecar("../../plugins/x.wasm", false)]
    #[case::safe_sidecar("font.ttf", true)]
    #[tokio::test]
    async fn an_mks_sidecar_is_judged_by_its_own_attachment_names(
        #[case] sidecar_name: &str,
        #[case] batch: bool,
    ) {
        let source = source_with(vec![attachment(3, Some("../main.ttf"))]);
        let encoder = FakeEncoder {
            sidecar: vec![attachment(0, Some(sidecar_name))],
            ..FakeEncoder::default()
        };
        let ex = extractor_with(
            encoder,
            Some(source.clone()),
            FakeIo::new(Some("/cache"), 1),
        );
        ex.extract_all_attachments("/media/movie.en.MKS", &source)
            .await
            .unwrap();
        assert_eq!(
            *ex.media_encoder.probed.lock().unwrap(),
            ["/media/movie.en.MKS"]
        );
        let args = ex.io.args().unwrap();
        if batch {
            assert!(args.starts_with("-dump_attachment:t \"\" -y "), "{args}");
            assert_eq!(ex.io.working_dir.lock().unwrap().as_deref(), Some("/cache"));
        } else {
            let tokens = crate::encoder::split_args(&args).unwrap();
            assert_eq!(
                tokens,
                [
                    "-dump_attachment:0",
                    "/cache/x.wasm",
                    "-i",
                    "file:/media/movie.en.MKS"
                ]
            );
            assert_eq!(ex.io.working_dir.lock().unwrap().as_deref(), None);
        }
    }

    /// The batch dump's existence check runs first, so a sidecar is probed
    /// only when something is left to extract.
    #[tokio::test]
    async fn an_extracted_mks_sidecar_is_not_probed() {
        let source = source_with(vec![attachment(3, Some("font.ttf"))]);
        let io = FakeIo::new(Some("/cache"), 0).with_files(&["/cache/font.ttf"]);
        let ex = extractor(Some(source.clone()), io);
        ex.extract_all_attachments("/media/movie.mks", &source)
            .await
            .unwrap();
        assert!(ex.media_encoder.probed.lock().unwrap().is_empty());
        assert_eq!(ex.io.args(), None);
    }

    /// A DVD or Blu-ray concat list is read with the concat demuxer, its
    /// `.concat` ending matched ignoring case as upstream does.
    #[rstest::rstest]
    #[case::batch("font.ttf")]
    #[case::per_attachment("../font.ttf")]
    #[tokio::test]
    async fn a_concat_input_gets_the_concat_demuxer_in_either_case(#[case] name: &str) {
        let source = source_with(vec![attachment(3, Some(name))]);
        let ex = extractor(Some(source.clone()), FakeIo::new(Some("/cache"), 0));
        ex.extract_all_attachments("/cache/concat/abc.CONCAT", &source)
            .await
            .unwrap();
        let args = ex.io.args().unwrap();
        assert!(
            args.contains("-f concat -safe 0 -i file:\"/cache/concat/abc.CONCAT\""),
            "{args}"
        );
    }

    /// Only a non-empty name that is more than its own leaf is unsafe: an
    /// absent or empty name never forces the per-attachment dump.
    #[rstest::rstest]
    #[case::absent(None, false)]
    #[case::empty(Some(""), false)]
    #[case::leaf(Some("font.ttf"), false)]
    #[case::dotted_leaf(Some(".hidden.ttf"), false)]
    #[case::subfolder(Some("a/font.ttf"), true)]
    // A separator only on Windows, there as in .NET's `Path.GetFileName`.
    #[case::backslash(Some("a\\font.ttf"), cfg!(windows))]
    #[case::parent(Some(".."), true)]
    fn which_names_are_unsafe(#[case] name: Option<&str>, #[case] unsafe_name: bool) {
        assert_eq!(
            AttachmentExtractorImpl::<FakeEncoder, FixedResolver, FakeIo>::has_unsafe_file_name(&[
                attachment(0, name)
            ]),
            unsafe_name
        );
    }

    /// A failed single extraction deletes the file it may have half-written,
    /// so the next request does not serve it as the attachment.
    #[tokio::test]
    async fn a_failed_single_extraction_deletes_its_output() {
        let mut source = source_with_attachment();
        source
            .media_streams
            .push(ferrofin_model::entities_media::MediaStream {
                stream_type: ferrofin_model::entities::MediaStreamType::Audio,
                ..ferrofin_model::entities_media::MediaStream::default()
            });
        let ex = extractor(Some(source), FakeIo::new(Some("/cache"), 1));
        let error = ex
            .get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap_err();
        assert!(matches!(error, ServiceError::Backend(_)), "{error:?}");
        assert_eq!(*ex.io.removed.lock().unwrap(), ["/cache/font.ttf"]);
    }

    /// A single attachment whose name has no leaf is cached under its index,
    /// as `GetAttachmentPath(id, FileName ?? index) ?? GetAttachmentPath(id,
    /// index)` falls back.
    #[tokio::test]
    async fn get_attachment_caches_a_leafless_name_under_its_index() {
        let source = source_with(vec![attachment(3, Some(".."))]);
        let ex = extractor(Some(source), FakeIo::new(Some("/cache"), 0));
        ex.get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
            .await
            .unwrap();
        assert!(
            ex.io
                .args()
                .unwrap()
                .starts_with("-dump_attachment:3 \"/cache/3\" -i ")
        );
    }

    /// A failed extraction names a remote source by its scheme and host:
    /// `ApiError` logs the error, and a `.strm` target can carry an account
    /// (gl-1121). Per attachment, batch and single.
    #[tokio::test]
    async fn a_failed_extraction_names_a_remote_source_by_its_host() {
        const URL: &str = "http://acct-user:info-secret@origin.example/live/path-secret/1.ts?password=query-secret";
        let remote = |mut source: MediaSourceInfo| {
            source.path = Some(URL.to_owned());
            source.protocol = MediaProtocol::Http;
            source
                .media_streams
                .push(ferrofin_model::entities_media::MediaStream {
                    stream_type: ferrofin_model::entities::MediaStreamType::Video,
                    ..ferrofin_model::entities_media::MediaStream::default()
                });
            source
        };
        let mut errors = Vec::new();
        for name in ["../a.ttf", "font.ttf"] {
            let source = remote(source_with(vec![attachment(3, Some(name))]));
            let ex = extractor(Some(source.clone()), FakeIo::new(Some("/cache"), 1));
            errors.push(ex.extract_all_attachments(URL, &source).await.unwrap_err());
        }
        let ex = extractor(
            Some(remote(source_with_attachment())),
            FakeIo::new(Some("/cache"), 1),
        );
        errors.push(
            ex.get_attachment(Uuid::nil(), "0f2c8d1e-1111-2222-3333-444455556666", 3)
                .await
                .unwrap_err(),
        );
        for error in errors {
            let text = error.to_string();
            assert!(text.contains("http://origin.example"), "{text}");
            for secret in ["acct-user", "info-secret", "path-secret", "query-secret"] {
                assert!(!text.contains(secret), "{secret} shown in: {text}");
            }
        }
    }
}
