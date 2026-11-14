//! The subtitle encoder: charset detection, format conversion, and extraction.
//!
//! Port of `MediaBrowser.MediaEncoding.Subtitles.SubtitleEncoder`. The pure
//! logic — `GetReadableFile`'s format decision, `ConvertSubtitles`/`FilterEvents`,
//! the `GetWriter` dispatch, and the charset-detect → UTF-8 conversion — is
//! ported here. The un-mockable I/O (reading files, HTTP, and the ffmpeg
//! subprocess) sits behind the [`SubtitleIo`] seam so unit tests inject a fake.
//!
//! Port substitutions:
//! - C# `UtfUnknown.CharsetDetector` → [`chardetng`] + [`encoding_rs`].
//! - `AsyncKeyedLock<string>` → a keyed [`tokio::sync::Mutex`] map (so
//!   `ConvertTextSubtitleToSrt` and extraction stay serialized per output path).
//! - `AsyncKeyedLock` around `ConvertSubtitles` determinism is preserved via the
//!   same keyed-lock helper.

use std::borrow::Cow;
use std::fmt::Write as _;

use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::IsoType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::{MediaProtocol, subtitle_format};
use ferrofin_traits::media_encoding::TrimmedSubtitle;

use crate::encoder::{get_input_argument, quote};
use crate::encoding_helper::helper;
use crate::error::MediaEncodingError;
use crate::keyed_locks::KeyedLocks;

use super::model::{Paragraph, Subtitle, TimeCode};
use super::parser::{SubtitleEditParser, SubtitleParser, unsupported_extension};
use super::{json_writer, srt, ssa, ttml, vtt};

/// The output format a [`GetWriter`](write)-selected writer emits.
///
/// Port of the `TryGetWriter` switch in the C# encoder, restricted to the
/// formats the writers support (`ass`, `ssa`, `srt`/`subrip`, `vtt`/`webvtt`,
/// `json`, `ttml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writer {
    /// Advanced SubStation Alpha writer.
    Ass,
    /// SubStation Alpha writer.
    Ssa,
    /// SubRip writer.
    SubRip,
    /// WebVTT writer.
    WebVtt,
    /// Jellyfin JSON writer.
    Json,
    /// TTML (`TimedText10`) writer.
    Ttml,
}

/// `subtitle` without the SSA/ASS `Comment:` events its loader keeps.
///
/// Deliberate divergence: libse loads a script's comments as cues flagged
/// `IsComment`, and only its SSA and ASS writers check the flag, so Jellyfin 12 shows
/// comments as ordinary subtitle lines in WebVTT, SubRip, TTML and JSON. That is a
/// Jellyfin bug (a script's comments are never displayed), so those writers get the
/// cues without them; the SSA/ASS loaders number the cues as if the comments were
/// absent. The SSA and ASS writers still write them back as `Comment:` lines.
fn without_comment_events(subtitle: &Subtitle) -> Cow<'_, Subtitle> {
    if subtitle.paragraphs.iter().any(|p| p.is_comment) {
        let mut kept = subtitle.clone();
        kept.paragraphs.retain(|p| !p.is_comment);
        Cow::Owned(kept)
    } else {
        Cow::Borrowed(subtitle)
    }
}

/// The title `SubtitleEncoder.ConvertSubtitles` passes to every writer
/// (`formatter.ToText(subtitle, "untitled")`).
const WRITER_TITLE: &str = "untitled";

impl Writer {
    /// Resolves the writer for `format`, if supported.
    ///
    /// Port of `SubtitleEncoder.TryGetWriter`.
    fn for_format(format: &str) -> Option<Self> {
        if format.eq_ignore_ascii_case(subtitle_format::ASS) {
            Some(Self::Ass)
        } else if format.eq_ignore_ascii_case("json") {
            Some(Self::Json)
        } else if format.eq_ignore_ascii_case(subtitle_format::SRT)
            || format.eq_ignore_ascii_case(subtitle_format::SUBRIP)
        {
            Some(Self::SubRip)
        } else if format.eq_ignore_ascii_case(subtitle_format::SSA) {
            Some(Self::Ssa)
        } else if format.eq_ignore_ascii_case(subtitle_format::VTT)
            || format.eq_ignore_ascii_case(subtitle_format::WEBVTT)
        {
            Some(Self::WebVtt)
        } else if format.eq_ignore_ascii_case(subtitle_format::TTML) {
            Some(Self::Ttml)
        } else {
            None
        }
    }

    /// The writer for `format`, or the `400` upstream's `ArgumentException`
    /// is. Port of `SubtitleEncoder.GetWriter`.
    fn get(format: &str) -> Result<Self, MediaEncodingError> {
        Self::for_format(format).ok_or_else(|| {
            MediaEncodingError::InvalidInput(format!("Unsupported format: {format}"))
        })
    }

    /// Writes `subtitle` in this writer's format.
    fn write(self, subtitle: &Subtitle) -> String {
        match self {
            Self::Ass => ssa::to_text_ass(subtitle, WRITER_TITLE),
            Self::Ssa => ssa::to_text_ssa(subtitle, WRITER_TITLE),
            Self::SubRip => srt::to_text(&without_comment_events(subtitle)),
            Self::WebVtt => vtt::to_text(&without_comment_events(subtitle)),
            Self::Json => json_writer::to_text(&without_comment_events(subtitle)),
            Self::Ttml => ttml::to_text(&without_comment_events(subtitle), WRITER_TITLE),
        }
    }
}

/// The longest delay, in milliseconds, .NET's `CancellationTokenSource.CancelAfter`
/// accepts: `Timer.MaxSupportedTimeout`, documented as `UInt32.MaxValue - 1`.
/// A longer delay makes it throw `ArgumentOutOfRangeException`.
const DOTNET_MAX_TIMER_DELAY_MS: u32 = u32::MAX - 1;

/// How long one subtitle extraction ffmpeg run may take before it is killed,
/// from `EncodingOptions.SubtitleExtractionTimeoutMinutes`.
///
/// Port of `waitSource.CancelAfter(TimeSpan.FromMinutes(timeoutMinutes))` in
/// `SubtitleEncoder.RunSubtitleExtractionProcess`, which reads the setting on
/// every run. Zero is a zero wait: upstream cancels at once, so ffmpeg is
/// killed and the extraction fails.
///
/// Deliberate divergence: a negative value, or one longer than
/// `DOTNET_MAX_TIMER_DELAY_MS` (71,582 minutes), makes upstream's
/// `CancelAfter` throw after ffmpeg has already started, so the extraction
/// fails and the orphaned ffmpeg keeps running. `None` here reports such a
/// value as invalid, so the caller fails the extraction without starting
/// ffmpeg.
#[must_use]
pub fn subtitle_extraction_timeout(minutes: i32) -> Option<std::time::Duration> {
    let timeout = std::time::Duration::from_mins(u64::try_from(minutes).ok()?);
    (timeout.as_millis() <= u128::from(DOTNET_MAX_TIMER_DELAY_MS)).then_some(timeout)
}

/// The un-mockable I/O the encoder depends on (files, HTTP, ffmpeg).
///
/// Everything the pure `SubtitleEncoder` logic cannot do without touching the
/// filesystem, network, or an ffmpeg subprocess is funnelled through this seam,
/// so unit tests substitute a deterministic fake and the real
/// `tokio::fs`/`tokio::process` calls stay out of the coverage/parity numbers.
#[async_trait]
pub trait SubtitleIo: Send + Sync {
    /// Reads the raw bytes at a local `path`. Port of `AsyncFile.OpenRead`.
    ///
    /// # Errors
    ///
    /// The I/O error, its kind intact: the encoder answers a missing file with
    /// the `404` upstream's `FileNotFoundException` is.
    async fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>>;

    /// Fetches the raw bytes of an HTTP(S) `url`. Port of the
    /// `IHttpClientFactory` `GetStreamAsync` calls.
    ///
    /// # Errors
    ///
    /// Returns an error message if the request fails.
    async fn http_get(&self, url: &str) -> Result<Vec<u8>, String>;

    /// The delivery protocol for `path`. Port of `IMediaSourceManager.GetPathProtocol`.
    fn path_protocol(&self, path: &str) -> MediaProtocol;

    /// The subtitle-cache path for a media-source id + stream index + extension,
    /// or `None` when the source has no GUID id. Port of
    /// `IPathManager.GetSubtitlePath`.
    fn subtitle_cache_path(
        &self,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_extension: &str,
    ) -> Option<String>;

    /// Runs an ffmpeg subtitle extraction/conversion, writing `output_paths`.
    /// Port of `RunSubtitleExtractionProcess`, which also returns what ffmpeg
    /// wrote to stderr: for a caller that finds an output empty, the end of
    /// it (empty when ffmpeg wrote nothing).
    ///
    /// # Errors
    ///
    /// Returns an error message, ending with that stderr, on ffmpeg failure,
    /// or when `args` does not split into argv (unbalanced quoting).
    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<String, String>;

    /// Whether a cached subtitle file already exists at `path`. Port of the
    /// `File.Exists` skip in `ExtractAllExtractableSubtitles` — without it every
    /// subtitle request re-runs the full-file ffmpeg extraction (seconds per
    /// request) instead of hitting the cache.
    fn file_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    /// Opens the local file at `path` and reads at most one byte: the
    /// failures [`read_file`](Self::read_file) would have opening it, without
    /// the file in memory. Port of the `AsyncFile.OpenRead` that upstream's
    /// `GetSubtitleStream` returns, which opens the file without reading it;
    /// the one-byte read refuses a directory, as .NET's open does.
    ///
    /// # Errors
    ///
    /// The I/O error, its kind intact, as [`read_file`](Self::read_file)
    /// returns it.
    async fn open_file(&self, path: &str) -> std::io::Result<()> {
        use tokio::io::AsyncReadExt as _;
        tokio::fs::File::open(path)
            .await?
            .read(&mut [0; 1])
            .await
            .map(drop)
    }

    /// Returns the size in bytes of the file at `path`, or `None` if the file
    /// does not exist or its metadata cannot be read.
    ///
    /// Used after extraction to verify that ffmpeg produced a non-empty output.
    fn file_size(&self, path: &str) -> Option<u64> {
        std::fs::metadata(path).ok().map(|m| m.len())
    }

    /// Removes the file at `path`. Returns `Ok` on success or if the file does
    /// not exist; returns `Err` with a message on other I/O failures.
    ///
    /// Port of the `_fileSystem.DeleteFile` calls in `ExtractSubtitlesForFile`
    /// that clean up missing/empty outputs so a zero-byte cache file does not
    /// prevent retry.
    ///
    /// # Errors
    ///
    /// Returns an error message when the file exists but cannot be deleted.
    fn delete_file(&self, path: &str) -> Result<(), String> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("delete {path}: {e}")),
        }
    }

    /// Writes `content` to the file at `path`, creating or truncating it.
    ///
    /// Used by the ASS font fix-up to rewrite a successfully extracted subtitle
    /// with `Arial Unicode MS` in place of `Arial`.
    ///
    /// # Errors
    ///
    /// Returns an error message if the write fails.
    fn write_file(&self, path: &str, content: &[u8]) -> Result<(), String> {
        std::fs::write(path, content).map_err(|e| format!("write {path}: {e}"))
    }

    /// The size in bytes and the last-write time, in .NET ticks (100 ns
    /// intervals since 0001-01-01 UTC), of the file at `path`, or `None` when
    /// it does not exist: the `FileInfo.Length`/`LastWriteTimeUtc.Ticks` pair a
    /// subtitle cache's `.meta` sidecar records, so a sidecar Jellyfin 12 wrote
    /// still matches.
    fn file_stamp(&self, path: &str) -> Option<(u64, i64)> {
        let metadata = std::fs::metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)?;
        let since_epoch = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let ticks = i64::try_from(since_epoch.as_nanos() / 100).ok()?;
        Some((metadata.len(), UNIX_EPOCH_TICKS + ticks))
    }
}

/// A failed read of `path`: [`MediaEncodingError::NotFound`] for what .NET
/// throws `FileNotFoundException` or `DirectoryNotFoundException` for (`ENOENT`
/// and `ENOTDIR` in `Interop.GetExceptionForIoErrno`; a `404` upstream), else
/// an I/O failure (`500`). The `404` names no path: its
/// message reaches the client, and the subtitle routes take anonymous callers.
fn read_error(path: &str, error: std::io::Error) -> MediaEncodingError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            // A 4xx is not logged where it is answered, so this is the one
            // place left that names the file.
            tracing::debug!(
                path = %ferrofin_util::source_url::redact_url(path),
                %error,
                "subtitle file not found"
            );
            MediaEncodingError::NotFound("subtitle file not found".to_owned())
        }
        _ => MediaEncodingError::io(format!("read {path}"), error),
    }
}

/// .NET ticks (100 ns intervals since 0001-01-01 UTC) at the Unix epoch.
const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;

/// `SubtitleEncoder.GetCacheMetaPath`: the sidecar recording which revision
/// of its source a subtitle cache file was built from.
fn cache_meta_path(cache_path: &str) -> String {
    format!("{cache_path}.meta")
}

/// `SubtitleEncoder.FormatCacheMeta`: `{size}:{ticks}`, as Jellyfin 12 writes it.
fn format_cache_meta((size, ticks): (u64, i64)) -> String {
    format!("{size}:{ticks}")
}

/// Resolved information about a readable subtitle file.
///
/// Port of `SubtitleEncoder.SubtitleInfo` (the nested record exposed for tests):
/// the resolved path, delivery protocol, parser format, and whether it is an
/// external (client-rendered) stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubtitleInfo {
    /// The resolved subtitle path.
    pub path: String,
    /// The delivery protocol.
    pub protocol: MediaProtocol,
    /// The parser/output format (e.g. `srt`, `ass`).
    pub format: String,
    /// Whether the stream is external (rendered by the client).
    pub is_external: bool,
}

/// Converts, extracts, and charset-normalizes subtitle streams.
///
/// Port of `SubtitleEncoder`. Generic over the injected [`SubtitleParser`] and
/// [`SubtitleIo`] seams so the pure logic is unit-testable with fakes.
pub struct SubtitleEncoder<P: SubtitleParser = SubtitleEditParser, I: SubtitleIo = NoopSubtitleIo> {
    parser: P,
    io: I,
    locks: KeyedLocks,
}

impl<P: SubtitleParser, I: SubtitleIo> SubtitleEncoder<P, I> {
    /// Creates an encoder from an injected parser and I/O seam.
    pub fn new(parser: P, io: I) -> Self {
        Self {
            parser,
            io,
            locks: KeyedLocks::default(),
        }
    }

    /// Produces a subtitle track in `output_format`, as bytes, for a time window.
    ///
    /// Port of `ISubtitleEncoder.GetSubtitles`: resolve the readable file
    /// ([`get_readable_file`](Self::get_readable_file)), read + charset-normalize
    /// it ([`get_subtitle_stream`](Self::get_subtitle_stream)), then — unless the
    /// stream is already in the requested format (ASS being a superset of SSA) —
    /// re-parse and re-emit it ([`convert_subtitles`](Self::convert_subtitles))
    /// over `[start, end]`. A file in a format the parser has no reader for is
    /// refused after it is opened, before it is read. The caller resolves the
    /// [`MediaSourceInfo`] and the target subtitle stream from the item +
    /// media-source id.
    ///
    /// # Errors
    ///
    /// [`MediaEncodingError::NotFound`] when the readable file is missing,
    /// [`MediaEncodingError::InvalidInput`] when `output_format` has no writer or
    /// the file no parser or no cue, else the failure resolving or reading it.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_subtitles(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
        output_format: &str,
        start_time_ticks: i64,
        end_time_ticks: i64,
        preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, MediaEncodingError> {
        let info = self
            .get_readable_file(media_source, subtitle_stream)
            .await?;

        // Return the original when the same format is requested. ASS is a
        // superset of SSA, so an SSA source satisfies an ASS request verbatim
        // (styles preserved). Character encoding was handled in
        // `get_subtitle_stream`.
        let same_format = info.format.eq_ignore_ascii_case(output_format)
            || (info.format.eq_ignore_ascii_case(subtitle_format::SSA)
                && output_format.eq_ignore_ascii_case(subtitle_format::ASS));

        // `Parse` refuses a format it has no parser for whatever the file
        // holds, so a bitmap track (megabytes of PGS or VobSub) is opened, as
        // upstream opens it, but not read into memory (gl-1495).
        if !same_format && !self.parser.supports_file_extension(&info.format) {
            self.io
                .open_file(&info.path)
                .await
                .map_err(|e| read_error(&info.path, e))?;
            return Err(unsupported_extension(&info.format));
        }

        let stream = self.get_subtitle_stream(&info).await?;
        if same_format {
            return Ok(stream.into_bytes());
        }

        self.convert_subtitles(
            stream.bytes(),
            &info,
            output_format,
            start_time_ticks,
            end_time_ticks,
            preserve_original_timestamps,
        )
        .await
    }

    /// Converts a parsed subtitle stream to `output_format` over a time window.
    ///
    /// Port of `SubtitleEncoder.ConvertSubtitles`: parse → [`filter_events`] →
    /// write. Serialized per stream key (`input_info.path`) so sequential and
    /// concurrent calls are deterministic, replacing the C# `AsyncKeyedLock`.
    ///
    /// # Errors
    ///
    /// [`MediaEncodingError::InvalidInput`] if parsing fails or `output_format`
    /// has no writer.
    pub async fn convert_subtitles(
        &self,
        data: &[u8],
        input_info: &SubtitleInfo,
        output_format: &str,
        start_time_ticks: i64,
        end_time_ticks: i64,
        preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, MediaEncodingError> {
        let guard = self.locks.get(&input_info.path);
        let _held = guard.lock().await;

        let mut subtitle = self.parser.parse(data, &input_info.format)?;

        filter_events(
            &mut subtitle,
            start_time_ticks,
            end_time_ticks,
            preserve_original_timestamps,
        );

        let writer = Writer::get(output_format)?;

        Ok(writer.write(&subtitle).into_bytes())
    }

    /// A text subtitle track cut to `start_time_ticks`, in the format of its
    /// readable file, for a stream that starts there and embeds it (gl-1199).
    ///
    /// Ferrofin's own, built from the ported steps: the readable file
    /// ([`get_readable_file`](Self::get_readable_file): the sidecar itself, or
    /// the SubRip ffmpeg made of one the parser cannot read), read as UTF-8
    /// ([`get_subtitle_stream`](Self::get_subtitle_stream); a sidecar in a
    /// legacy character set is converted, where ffmpeg reading it itself
    /// assumes UTF-8), its cues from the
    /// start position ([`filter_events`], moved to start there), written back
    /// in that format. [`get_subtitles`](Self::get_subtitles) returns a track
    /// already in the requested format uncut, as upstream's `GetSubtitles`
    /// does.
    ///
    /// Divergence, the one the embed's output trim makes from ffmpeg 5.1
    /// (`embedded_sidecar_trim_arguments`): a cue that ended exactly at the
    /// start goes too. `FilterEvents` keeps it with no length, which shows
    /// nothing, but MP4's `mov_text` would stretch it to the next cue.
    ///
    /// A start after the last cue leaves none. SubRip has no header, and
    /// ffmpeg refuses a SubRip file without a cue ("Invalid data found when
    /// processing input", 4.4.2 and 9.0.2), which would fail the whole
    /// stream, so that copy holds one cue with no text at 0: ffmpeg reads it
    /// as no subtitle, and the embedded track is empty, as the output trim
    /// leaves it from 5.1. ASS, SSA and WebVTT keep their header and open
    /// with no cue.
    ///
    /// # Errors
    ///
    /// [`MediaEncodingError::NotFound`] when the readable file is missing,
    /// [`MediaEncodingError::InvalidInput`] when it cannot be parsed or its
    /// format has no writer (a bitmap track), else the failure resolving or
    /// reading it.
    pub async fn get_trimmed_subtitles(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
        start_time_ticks: i64,
    ) -> Result<TrimmedSubtitle, MediaEncodingError> {
        let info = self
            .get_readable_file(media_source, subtitle_stream)
            .await?;
        let writer = Writer::get(&info.format)?;
        let stream = self.get_subtitle_stream(&info).await?;

        let guard = self.locks.get(&info.path);
        let _held = guard.lock().await;
        let mut subtitle = self.parser.parse(stream.bytes(), &info.format)?;
        subtitle.paragraphs.retain(|p| {
            !(p.start_time.ticks() < start_time_ticks && p.end_time.ticks() <= start_time_ticks)
        });
        filter_events(&mut subtitle, start_time_ticks, 0, false);
        if subtitle.paragraphs.is_empty() && writer == Writer::SubRip {
            let mut blank = Paragraph::new(String::new(), TimeCode::default(), TimeCode::default());
            blank.number = 1;
            subtitle.paragraphs.push(blank);
        }

        Ok(TrimmedSubtitle {
            data: writer.write(&subtitle).into_bytes(),
            format: info.format,
        })
    }

    /// Reads a subtitle stream, converting a non-UTF-8 file to UTF-8 in memory.
    ///
    /// Port of `SubtitleEncoder.GetSubtitleStream(SubtitleInfo, …)`: external
    /// text streams are charset-detected; already-UTF-8/ASCII content is served
    /// verbatim (the `Utf8Preserved` variant), otherwise it is decoded and
    /// re-encoded as UTF-8 (`Converted`).
    ///
    /// # Errors
    ///
    /// [`MediaEncodingError::NotFound`] when the file is missing, else the
    /// failed read or HTTP fetch.
    pub async fn get_subtitle_stream(
        &self,
        file_info: &SubtitleInfo,
    ) -> Result<SubtitleStream, MediaEncodingError> {
        if file_info.is_external && MediaStream::is_text_format(Some(&file_info.format)) {
            let bytes = self.read_bytes(&file_info.path, file_info.protocol).await?;

            match detect_charset(&bytes) {
                DetectedCharset::Utf8OrAscii => Ok(SubtitleStream::new(bytes, false)),
                DetectedCharset::Other(encoding) => {
                    let (text, _, _) = encoding.decode(&bytes);
                    Ok(SubtitleStream::new(text.into_owned().into_bytes(), true))
                }
            }
        } else {
            let bytes = self
                .io
                .read_file(&file_info.path)
                .await
                .map_err(|e| read_error(&file_info.path, e))?;
            Ok(SubtitleStream::new(bytes, false))
        }
    }

    /// `SubtitleEncoder.IsCachedSubtitleFresh`: whether the cache file at
    /// `cache_path` can be served for `source_path`. A missing or empty cache
    /// is stale and a missing source keeps the cache. Otherwise the cache's
    /// `.meta` sidecar must record the source's current size and last-write
    /// time, so a cache from before the sidecar existed is rebuilt once.
    async fn is_cached_subtitle_fresh(&self, cache_path: &str, source_path: &str) -> bool {
        if !self.io.file_exists(cache_path) || self.io.file_size(cache_path) == Some(0) {
            return false;
        }
        let Some(stamp) = Some(source_path)
            .filter(|path| !path.is_empty())
            .and_then(|path| self.io.file_stamp(path))
        else {
            return true;
        };
        self.io
            .read_file(&cache_meta_path(cache_path))
            .await
            .is_ok_and(|recorded| recorded == format_cache_meta(stamp).as_bytes())
    }

    /// `SubtitleEncoder.WriteCacheMeta`: records `source_path`'s current size
    /// and last-write time beside the cache file built from it.
    fn write_cache_meta(&self, cache_path: &str, source_path: &str) {
        let Some(stamp) = Some(source_path)
            .filter(|path| !path.is_empty())
            .and_then(|path| self.io.file_stamp(path))
        else {
            return;
        };
        if let Err(err) = self.io.write_file(
            &cache_meta_path(cache_path),
            format_cache_meta(stamp).as_bytes(),
        ) {
            tracing::warn!(cache_path, %err, "failed to record subtitle cache metadata");
        }
    }

    /// Resolves the on-disk (or extracted) path of a subtitle stream.
    ///
    /// Port of `SubtitleEncoder.GetSubtitleFilePath`: the readable-file path
    /// (extraction runs first when the stream is embedded/`.mks`).
    ///
    /// # Errors
    ///
    /// The [`get_readable_file`](Self::get_readable_file) failure.
    pub async fn get_subtitle_file_path(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
    ) -> Result<String, MediaEncodingError> {
        Ok(self
            .get_readable_file(media_source, subtitle_stream)
            .await?
            .path)
    }

    /// Detects the character-set name ffmpeg should be told to decode a subtitle
    /// stream with.
    ///
    /// Port of `SubtitleEncoder.GetSubtitleFileCharacterSet`: an already-UTF-8
    /// (or UTF-16, which ffmpeg auto-converts) `.ass`/`.ssa`/`.srt` needs no
    /// charset hint and yields the empty string; otherwise the detected legacy
    /// encoding name (e.g. `windows-1252`) is returned. The `.mks` extraction
    /// branch of the C# is handled by the caller resolving the path first.
    ///
    /// # Errors
    ///
    /// [`MediaEncodingError::NotFound`] when the file is missing, else the
    /// failed read or HTTP fetch.
    pub async fn get_subtitle_file_character_set(
        &self,
        subtitle_stream: &MediaStream,
    ) -> Result<String, MediaEncodingError> {
        let path = subtitle_stream.path.clone().unwrap_or_default();
        let protocol = self.io.path_protocol(&path);
        let bytes = self.read_bytes(&path, protocol).await?;

        let charset = match detect_charset(&bytes) {
            DetectedCharset::Utf8OrAscii => String::new(),
            DetectedCharset::Other(encoding) => encoding.name().to_ascii_lowercase(),
        };

        // UTF-16 is auto-converted to UTF-8 by ffmpeg for these containers, so no
        // explicit character encoding should be specified.
        if (ends_with_ignore_ascii_case(&path, ".ass")
            || ends_with_ignore_ascii_case(&path, ".ssa")
            || ends_with_ignore_ascii_case(&path, ".srt"))
            && (charset == "utf-16le" || charset == "utf-16be")
        {
            return Ok(String::new());
        }

        Ok(charset)
    }

    /// Reads bytes from a path over its protocol (HTTP or local file).
    async fn read_bytes(
        &self,
        path: &str,
        protocol: MediaProtocol,
    ) -> Result<Vec<u8>, MediaEncodingError> {
        if protocol == MediaProtocol::Http {
            Ok(self.io.http_get(path).await?)
        } else {
            self.io
                .read_file(path)
                .await
                .map_err(|e| read_error(path, e))
        }
    }

    /// Resolves the readable file (path/protocol/format) for a subtitle stream.
    ///
    /// Port of `SubtitleEncoder.GetReadableFile`, restricted to the ported
    /// (software) path: external parser-supported and PGS streams pass through;
    /// unsupported external text is converted to `.srt` via ffmpeg; embedded /
    /// `.mks` streams are extracted first.
    ///
    /// # Errors
    ///
    /// [`MediaEncodingError::NotFound`] when the media source has no subtitle
    /// cache (upstream's `ResourceNotFoundException`) or the sidecar to convert
    /// is missing, [`MediaEncodingError::InvalidInput`] when that sidecar has no
    /// path, else the failed ffmpeg conversion.
    pub async fn get_readable_file(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
    ) -> Result<SubtitleInfo, MediaEncodingError> {
        let stream_path = subtitle_stream.path.clone().unwrap_or_default();

        if !subtitle_stream.is_external || ends_with_ignore_ascii_case(&stream_path, ".mks") {
            self.extract_all_extractable_subtitles(media_source).await;

            let output_file_extension = get_extractable_subtitle_file_extension(subtitle_stream);
            let output_format = get_extractable_subtitle_format(subtitle_stream);
            let output_path = self
                .subtitle_cache_path(media_source, subtitle_stream.index, &output_file_extension)
                .ok_or_else(|| {
                    MediaEncodingError::NotFound(format!(
                        "MediaSource {} has no subtitle cache (non-GUID Id, e.g. Live TV stream).",
                        media_source.id.clone().unwrap_or_default()
                    ))
                })?;

            return Ok(SubtitleInfo {
                path: output_path,
                protocol: MediaProtocol::File,
                is_external: MediaStream::is_vob_sub_format(Some(&output_format)),
                format: output_format,
            });
        }

        // Normalize ffmpeg codec names to the file extensions the parser is keyed on.
        let extension_source = path_extension(&stream_path)
            .unwrap_or_else(|| subtitle_stream.codec.clone().unwrap_or_default());
        let current_format = normalize_codec_to_parser_extension(&extension_source);

        // Handle PGS subtitles as raw streams for the client to render.
        if MediaStream::is_pgs_format(Some(&current_format)) {
            return Ok(SubtitleInfo {
                path: stream_path.clone(),
                protocol: self.io.path_protocol(&stream_path),
                format: "pgssub".to_owned(),
                is_external: true,
            });
        }

        // Fallback to ffmpeg conversion for unsupported text formats.
        if !self.parser.supports_file_extension(&current_format) {
            let output_path = self
                .subtitle_cache_path(media_source, subtitle_stream.index, "srt")
                .ok_or_else(|| {
                    MediaEncodingError::NotFound(format!(
                        "MediaSource {} has no subtitle cache (non-GUID Id, e.g. Live TV stream).",
                        media_source.id.clone().unwrap_or_default()
                    ))
                })?;

            self.convert_text_subtitle_to_srt(subtitle_stream, &output_path)
                .await?;

            return Ok(SubtitleInfo {
                path: output_path,
                protocol: MediaProtocol::File,
                format: subtitle_format::SRT.to_owned(),
                is_external: true,
            });
        }

        Ok(SubtitleInfo {
            path: stream_path.clone(),
            protocol: self.io.path_protocol(&stream_path),
            format: current_format,
            is_external: true,
        })
    }

    /// Extracts every extractable subtitle from a media source via ffmpeg.
    ///
    /// Port of `SubtitleEncoder.ExtractAllExtractableSubtitles`: the embedded
    /// streams in one ffmpeg run over the video file, then each external
    /// `.mks` file in its own (`Self::extract_all_extractable_subtitles_mks`);
    /// each output cache path is serialized with the keyed lock. Errors are
    /// swallowed (logged in C#) so a single un-extractable stream cannot abort
    /// the caller.
    pub async fn extract_all_extractable_subtitles(&self, media_source: &MediaSourceInfo) {
        let source_path = media_source.path.clone().unwrap_or_default();
        // TODO(parity): upstream's `GetInputPathArgument` also picks a disc rip's
        // playlist files, the `bluray:` prefix for an ISO and the source protocol.
        let input_path = get_input_argument("file", &source_path, MediaProtocol::File);
        let mut args = format!("-y -i {input_path}");
        let mut output_paths: Vec<String> = Vec::new();
        // Guards for the paths this call extracts, held across the ffmpeg run so
        // concurrent extractions of the same cache never race (the C# per-path
        // `AsyncKeyedLock`).
        let mut held = Vec::new();

        for stream in &media_source.media_streams {
            if !stream.is_extractable_subtitle_stream() {
                continue;
            }
            let stream_path = stream.path.clone().unwrap_or_default();
            if stream.is_external && !ends_with_ignore_ascii_case(&stream_path, ".mks") {
                continue;
            }
            if !stream_path.is_empty() && ends_with_ignore_ascii_case(&stream_path, ".mks") {
                continue;
            }
            let ext = get_extractable_subtitle_file_extension(stream);
            let Some(output_path) = self.subtitle_cache_path(media_source, stream.index, &ext)
            else {
                continue;
            };
            // `GetSubtitleStreamIndexForFfmpeg`: the track's place among the
            // video file's own streams, not among every stream of the source
            // (a sidecar listed first would shift it past the end of the file).
            let stream_index = helper::get_subtitle_stream_index_for_ffmpeg(media_source, stream);
            if stream_index == -1 {
                tracing::error!(
                    path = %ferrofin_util::source_url::redact_url(&source_path),
                    index = stream.index,
                    "cannot find subtitle stream index, skipping this stream"
                );
                continue;
            }
            // Lock the output path, then skip a fresh cache
            // (`IsCachedSubtitleFresh` against the stream's own file, else the
            // media file). Without it every subtitle request re-runs the
            // full-file ffmpeg extraction (seconds per request), and a media
            // file replaced in place keeps serving the old extraction. Taking
            // the lock first also means a concurrent extraction of the same
            // path completes before the check.
            let guard = self.locks.get(&output_path).lock_owned().await;
            let cache_source = if stream_path.is_empty() {
                source_path.as_str()
            } else {
                stream_path.as_str()
            };
            if self
                .is_cached_subtitle_fresh(&output_path, cache_source)
                .await
            {
                continue;
            }
            held.push(guard);
            let codec = stream.codec.clone().unwrap_or_default();
            let output_codec = if is_codec_copyable(&codec) {
                "copy"
            } else {
                "srt"
            };
            let output_format_option = if MediaStream::is_vob_sub_format(Some(&codec)) {
                " -f matroska"
            } else {
                ""
            };
            output_paths.push(output_path.clone());
            let _ = write!(
                args,
                " -map 0:{stream_index} -an -vn -c:s {output_codec}{output_format_option} -flush_packets 1 {}",
                quote(&output_path)
            );
        }

        if !output_paths.is_empty() {
            let result = self.io.extract(&args, &output_paths).await;
            let extracted = result.is_ok();
            self.validate_extraction(&source_path, &output_paths, result)
                .await;
            if extracted {
                for output_path in &output_paths {
                    self.write_cache_meta(output_path, &source_path);
                }
            }
        }
        drop(held);

        self.extract_all_extractable_subtitles_mks(media_source)
            .await;
    }

    /// The external `.mks` half of [`Self::extract_all_extractable_subtitles`].
    ///
    /// Port of `SubtitleEncoder.ExtractAllExtractableSubtitlesMKS`: one ffmpeg
    /// run per `.mks` file, each of its tracks mapped by its position inside
    /// that file (`EncodingHelper.FindIndex`) and a VobSub track re-muxed as
    /// Matroska, since ffmpeg has no `.idx`/`.sub` muxer. Each output is locked
    /// and a cache hit skipped, as in the embedded pass.
    async fn extract_all_extractable_subtitles_mks(&self, media_source: &MediaSourceInfo) {
        let streams: Vec<&MediaStream> = media_source
            .media_streams
            .iter()
            .filter(|stream| stream.is_extractable_subtitle_stream() && mks_path(stream).is_some())
            .collect();
        let mut files: Vec<&str> = Vec::new();
        for file in streams.iter().filter_map(|stream| mks_path(stream)) {
            if !files.contains(&file) {
                files.push(file);
            }
        }

        for file in files {
            // `MediaEncoder.GetInputArgument(mksFile, mediaSource)`.
            let prefix = if media_source.iso_type == Some(IsoType::BluRay) {
                "bluray"
            } else {
                "file"
            };
            let input_path = get_input_argument(prefix, file, media_source.protocol);
            let mut args = format!("-y -i {input_path}");
            let mut output_paths: Vec<String> = Vec::new();
            let mut held = Vec::new();
            for &stream in streams.iter().filter(|stream| {
                mks_path(stream).is_some_and(|path| path.eq_ignore_ascii_case(file))
            }) {
                let ext = get_extractable_subtitle_file_extension(stream);
                let Some(output_path) = self.subtitle_cache_path(media_source, stream.index, &ext)
                else {
                    continue;
                };
                let stream_index = helper::find_index(&media_source.media_streams, stream);
                if stream_index == -1 {
                    tracing::error!(
                        path = %file,
                        index = stream.index,
                        "cannot find subtitle stream index, skipping this stream"
                    );
                    continue;
                }
                let guard = self.locks.get(&output_path).lock_owned().await;
                if self.is_cached_subtitle_fresh(&output_path, file).await {
                    continue;
                }
                held.push(guard);
                let codec = stream.codec.clone().unwrap_or_default();
                let output_codec = if is_codec_copyable(&codec) {
                    "copy"
                } else {
                    "srt"
                };
                let output_format_option = if MediaStream::is_vob_sub_format(Some(&codec)) {
                    " -f matroska"
                } else {
                    ""
                };
                output_paths.push(output_path.clone());
                let _ = write!(
                    args,
                    " -map 0:{stream_index} -an -vn -c:s {output_codec}{output_format_option} -flush_packets 1 {}",
                    quote(&output_path)
                );
            }
            if output_paths.is_empty() {
                continue;
            }
            let result = self.io.extract(&args, &output_paths).await;
            let extracted = result.is_ok();
            self.validate_extraction(file, &output_paths, result).await;
            if extracted {
                for output_path in &output_paths {
                    self.write_cache_meta(output_path, file);
                }
            }
            drop(held);
        }
    }

    /// Validates and cleans up the outputs of a subtitle extraction.
    ///
    /// Port of `SubtitleEncoder.ExtractSubtitlesForFile`: on ffmpeg failure,
    /// every output is deleted; on success, each output is checked individually
    /// — missing/empty ones are logged with ffmpeg's stderr and deleted,
    /// successful `.ass` outputs get the [`set_ass_font`](Self::set_ass_font)
    /// fix-up.
    async fn validate_extraction(
        &self,
        source_path: &str,
        output_paths: &[String],
        result: Result<String, String>,
    ) {
        // A `.strm` source's path is its target URL, which can carry an account.
        let source_path = ferrofin_util::source_url::redact_url(source_path);
        match result {
            Err(error) => {
                // Extraction failed — delete all outputs so zero-byte cache
                // files don't prevent retry on the next request. Port of
                // upstream's `exitCode == -1` branch.
                for path in output_paths {
                    tracing::warn!(path = %path, "deleting extracted subtitle due to failure");
                    if let Err(e) = self.io.delete_file(path) {
                        tracing::error!(error = %e, path = %path, "error deleting extracted subtitle");
                    }
                }
                tracing::error!(path = %source_path, %error, "ffmpeg subtitle extraction failed");
            }
            Ok(stderr) => {
                // Check each output individually — ffmpeg may exit 0 yet
                // still fail to produce some outputs.
                let mut failed = false;
                for path in output_paths {
                    match self.io.file_size(path) {
                        Some(size) if size > 0 => {
                            if ends_with_ignore_ascii_case(path, ".ass") {
                                self.set_ass_font(path).await;
                            }
                            tracing::info!(
                                input = %source_path, output = %path,
                                "ffmpeg subtitle extraction completed"
                            );
                        }
                        _ => {
                            failed = true;
                            tracing::error!(
                                input = %source_path, output = %path,
                                "ffmpeg subtitle extraction failed"
                            );
                            tracing::warn!(path = %path, "deleting extracted subtitle due to failure");
                            if let Err(e) = self.io.delete_file(path) {
                                tracing::error!(error = %e, path = %path, "error deleting extracted subtitle");
                            }
                        }
                    }
                }
                // Once per run, after the outputs, as upstream logs `{FfmpegOutput}`.
                if failed && !stderr.is_empty() {
                    tracing::error!(input = %source_path, %stderr, "ffmpeg subtitle extraction failed");
                }
            }
        }
    }

    /// Converts an unsupported text subtitle to SRT via ffmpeg, once per path.
    ///
    /// Port of `ConvertTextSubtitleToSrt`: guarded by the keyed lock so a given
    /// output path is only converted by one caller at a time.
    async fn convert_text_subtitle_to_srt(
        &self,
        subtitle_stream: &MediaStream,
        output_path: &str,
    ) -> Result<(), MediaEncodingError> {
        let guard = self.locks.get(output_path);
        let _held = guard.lock().await;

        let input_path = subtitle_stream.path.clone().unwrap_or_default();
        // `ConvertTextSubtitleToSrt`: a conversion of this revision of the
        // sidecar is reused instead of running ffmpeg on every request.
        if self
            .is_cached_subtitle_fresh(output_path, &input_path)
            .await
        {
            return Ok(());
        }
        if input_path.is_empty() {
            return Err(MediaEncodingError::InvalidInput(
                "inputPath is empty".to_owned(),
            ));
        }

        // Tell ffmpeg the source charset (C# `GetSubtitleFileCharacterSet`), or
        // it decodes a legacy-encoded file as UTF-8 and mangles every non-ASCII
        // character. ffmpeg auto-converts UTF-16 `.smi`/`.sami` itself and
        // rejects an explicit `-sub_charenc` for them, so those stay unset.
        let charset = self
            .get_subtitle_file_character_set(subtitle_stream)
            .await?;
        let is_utf16_smi = (ends_with_ignore_ascii_case(&input_path, ".smi")
            || ends_with_ignore_ascii_case(&input_path, ".sami"))
            && (charset.eq_ignore_ascii_case("utf-16be")
                || charset.eq_ignore_ascii_case("utf-16le"));
        let encoding_param = if charset.is_empty() || is_utf16_smi {
            String::new()
        } else {
            format!(" -sub_charenc {charset}")
        };

        let args = format!(
            "-y{encoding_param} -i {} -c:s srt {}",
            quote(&input_path),
            quote(output_path)
        );
        let stderr = self
            .io
            .extract(&args, std::slice::from_ref(&output_path.to_owned()))
            .await?;

        // Verify the output was written and is non-empty (port of upstream's
        // per-output check in `ExtractSubtitlesForFile`, also reached via
        // `ExtractTextSubtitleInternal`).
        match self.io.file_size(output_path) {
            Some(size) if size > 0 => {
                self.write_cache_meta(output_path, &input_path);
                Ok(())
            }
            _ => {
                let _ = self.io.delete_file(output_path);
                let mut message =
                    format!("ffmpeg subtitle conversion produced no output for {output_path}");
                if !stderr.is_empty() {
                    let _ = write!(message, ": {stderr}");
                }
                Err(MediaEncodingError::Process(message))
            }
        }
    }

    /// Replaces `,Arial,` with `,Arial Unicode MS,` in a successfully extracted
    /// `.ass` subtitle file.
    ///
    /// Port of `SubtitleEncoder.SetAssFont` (lines 932-966): ffmpeg-extracted
    /// `.ass` files may reference `Arial`, which lacks glyphs for CJK and many
    /// Unicode symbols. `Arial Unicode MS` is a wide-coverage fallback that
    /// renders them correctly.
    async fn set_ass_font(&self, path: &str) {
        let Ok(bytes) = self.io.read_file(path).await else {
            tracing::warn!(path = %path, "could not read .ass file for font fix-up");
            return;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return;
        };
        if let Some(replaced) = set_ass_font_text(text) {
            tracing::info!(path = %path, "setting ASS font within extracted subtitle");
            if let Err(e) = self.io.write_file(path, replaced.as_bytes()) {
                tracing::warn!(error = %e, path = %path, "could not write ASS font fix-up");
            }
        }
    }

    /// Resolves the subtitle-cache path for a media source, stream, extension.
    fn subtitle_cache_path(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream_index: i32,
        output_extension: &str,
    ) -> Option<String> {
        let id = media_source.id.clone().unwrap_or_default();
        self.io
            .subtitle_cache_path(&id, subtitle_stream_index, &format!(".{output_extension}"))
    }
}

/// A read subtitle stream: its bytes plus whether it was charset-converted.
///
/// Port of the return of `GetSubtitleStream`. The C# code distinguishes a
/// disk-backed `FileStream` (short-circuited, already UTF-8) from an in-memory
/// `MemoryStream` (converted); [`Self::is_converted`] carries that distinction
/// for the `IsNotType<MemoryStream>` oracle assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleStream {
    bytes: Vec<u8>,
    converted: bool,
}

impl SubtitleStream {
    /// Creates a stream from its bytes and whether it was charset-converted.
    fn new(bytes: Vec<u8>, converted: bool) -> Self {
        Self { bytes, converted }
    }

    /// The (UTF-8) subtitle bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the stream, returning its bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether the bytes were decoded from a legacy charset and re-encoded as
    /// UTF-8 (i.e. an in-memory `MemoryStream` in the C# code).
    #[must_use]
    pub fn is_converted(&self) -> bool {
        self.converted
    }
}

/// A no-op [`SubtitleIo`] used as the default seam when none is injected.
///
/// Every method fails or returns the empty result; real deployments and tests
/// supply a concrete implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSubtitleIo;

#[async_trait]
impl SubtitleIo for NoopSubtitleIo {
    async fn read_file(&self, _path: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("no subtitle I/O configured"))
    }

    async fn open_file(&self, _path: &str) -> std::io::Result<()> {
        Err(std::io::Error::other("no subtitle I/O configured"))
    }

    async fn http_get(&self, _url: &str) -> Result<Vec<u8>, String> {
        Err("no subtitle I/O configured".to_owned())
    }

    fn path_protocol(&self, path: &str) -> MediaProtocol {
        path_protocol(path)
    }

    fn subtitle_cache_path(
        &self,
        _media_source_id: &str,
        _subtitle_stream_index: i32,
        _output_extension: &str,
    ) -> Option<String> {
        None
    }

    async fn extract(&self, _args: &str, _output_paths: &[String]) -> Result<String, String> {
        Err("no subtitle I/O configured".to_owned())
    }
}

/// Filters a track's cues to a `[start, end]` window, shifting timestamps.
///
/// Port of `SubtitleEncoder.FilterEvents`: drops cues that fully elapse before
/// `start_position_ticks`, drops cues starting after `end_time_ticks` (when
/// positive), and — unless `preserve_timestamps` — rebases every remaining cue
/// so `start` becomes the new zero.
pub fn filter_events(
    track: &mut Subtitle,
    start_position_ticks: i64,
    end_time_ticks: i64,
    preserve_timestamps: bool,
) {
    track.paragraphs.retain(|p| {
        // C#'s `long` arithmetic is unchecked; the offset comes from the request.
        !(p.start_time.ticks().wrapping_sub(start_position_ticks) < 0
            && p.end_time.ticks().wrapping_sub(start_position_ticks) < 0)
    });

    if end_time_ticks > 0 {
        track
            .paragraphs
            .retain(|p| p.start_time.ticks() <= end_time_ticks);
    }

    if !preserve_timestamps {
        for p in &mut track.paragraphs {
            p.start_time = TimeCode::from_ticks(
                p.start_time
                    .ticks()
                    .wrapping_sub(start_position_ticks)
                    .max(0),
            );
            p.end_time =
                TimeCode::from_ticks(p.end_time.ticks().wrapping_sub(start_position_ticks).max(0));
        }
    }
}

/// The result of running the charset detector over subtitle bytes.
enum DetectedCharset {
    /// The bytes are already UTF-8 or ASCII (serve verbatim).
    Utf8OrAscii,
    /// A legacy encoding to decode from.
    Other(&'static encoding_rs::Encoding),
}

/// Detects the character set of `bytes`, replacing C# `UtfUnknown.CharsetDetector`.
///
/// A UTF-8/UTF-16 BOM or valid-UTF-8 content without an ESC byte short-circuits
/// to UTF-8; otherwise [`chardetng`] guesses the encoding, ISO-2022-JP first.
/// UTF-16 (with BOM) decodes through `encoding_rs` so wide files round-trip.
fn detect_charset(bytes: &[u8]) -> DetectedCharset {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return DetectedCharset::Utf8OrAscii;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return DetectedCharset::Other(encoding_rs::UTF_16LE);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return DetectedCharset::Other(encoding_rs::UTF_16BE);
    }
    // BOM-less UTF-16 must be ruled out BEFORE the valid-UTF-8 shortcut:
    // mostly-ASCII UTF-16 text is also valid UTF-8 (NUL bytes are legal), and
    // serving it verbatim renders every char NUL-interleaved — e.g. ♪ (U+266A)
    // in a BOM-less UTF-16LE subtitle displays as the raw bytes "j&". The C#
    // `UtfUnknown.CharsetDetector` detects this case; `chardetng` does not.
    if let Some(encoding) = detect_bomless_utf16(bytes) {
        return DetectedCharset::Other(encoding);
    }
    // 7-bit ISO-2022-JP is valid UTF-8 too, and `UtfUnknown`'s escape prober
    // finds it first: an ESC, which starts its designations, leaves the choice
    // to chardetng. It answers ISO-2022-JP only for ASCII that decodes as it,
    // and UTF-8 for any other valid UTF-8. TODO(gl-1657): the prober's other
    // 7-bit encodings, ISO-2022-KR and HZ-GB-2312, are not detected yet.
    if !bytes.contains(&0x1B) && std::str::from_utf8(bytes).is_ok() {
        return DetectedCharset::Utf8OrAscii;
    }

    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    if encoding == encoding_rs::UTF_8 {
        DetectedCharset::Utf8OrAscii
    } else {
        DetectedCharset::Other(encoding)
    }
}

/// Detects BOM-less UTF-16 text by its null-byte pattern (the UTF-16 probers of
/// C# `UtfUnknown`): predominantly-ASCII UTF-16 has a NUL high byte in most
/// code units and none in the low bytes. Text without that shape (e.g. CJK
/// UTF-16, any 8-bit encoding, real UTF-8) returns `None` and falls through to
/// the ordinary detection.
fn detect_bomless_utf16(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let pairs = bytes.len() / 2;
    // Too short to establish a pattern (matches the detector's reluctance to
    // classify tiny buffers).
    if pairs < 4 {
        return None;
    }
    let mut even_nuls = 0usize;
    let mut odd_nuls = 0usize;
    for pair in bytes.as_chunks::<2>().0 {
        even_nuls += usize::from(pair[0] == 0);
        odd_nuls += usize::from(pair[1] == 0);
    }
    // "Most" high bytes NUL, and strictly none on the other side — the
    // asymmetry is what distinguishes UTF-16 ASCII text from binary noise.
    if odd_nuls > pairs / 2 && even_nuls == 0 {
        return Some(encoding_rs::UTF_16LE);
    }
    if even_nuls > pairs / 2 && odd_nuls == 0 {
        return Some(encoding_rs::UTF_16BE);
    }
    None
}

/// The delivery protocol for `path`. Port of `MediaSourceManager.GetPathProtocol`.
fn path_protocol(path: &str) -> MediaProtocol {
    if path.is_empty() {
        MediaProtocol::File
    } else if starts_with_ignore_ascii_case(path, "rtsp") {
        MediaProtocol::Rtsp
    } else if starts_with_ignore_ascii_case(path, "rtmp") {
        MediaProtocol::Rtmp
    } else if starts_with_ignore_ascii_case(path, "http") {
        MediaProtocol::Http
    } else {
        MediaProtocol::File
    }
}

/// Maps an ffmpeg codec name to the extension the parser is keyed on.
///
/// Port of `SubtitleEncoder.NormalizeCodecToParserExtension`.
fn normalize_codec_to_parser_extension(codec_or_extension: &str) -> String {
    match codec_or_extension {
        "subrip" => "srt".to_owned(),
        "webvtt" => "vtt".to_owned(),
        other => other.to_owned(),
    }
}

/// The extraction file extension for a stream. Port of
/// `GetExtractableSubtitleFileExtension`.
fn get_extractable_subtitle_file_extension(subtitle_stream: &MediaStream) -> String {
    let codec = subtitle_stream.codec.clone().unwrap_or_default();
    if codec.eq_ignore_ascii_case("pgssub") {
        "sup".to_owned()
    } else if MediaStream::is_vob_sub_format(Some(&codec)) {
        "mks".to_owned()
    } else {
        get_extractable_subtitle_format(subtitle_stream)
    }
}

/// The extraction format for a stream. Port of `GetExtractableSubtitleFormat`.
fn get_extractable_subtitle_format(subtitle_stream: &MediaStream) -> String {
    let codec = subtitle_stream.codec.clone().unwrap_or_default();
    if codec.eq_ignore_ascii_case("ass")
        || codec.eq_ignore_ascii_case("ssa")
        || codec.eq_ignore_ascii_case("pgssub")
    {
        codec
    } else if MediaStream::is_vob_sub_format(Some(&codec)) {
        "mks".to_owned()
    } else {
        "srt".to_owned()
    }
}

/// The `.mks` file a subtitle stream lives in, if it is one.
fn mks_path(stream: &MediaStream) -> Option<&str> {
    stream
        .path
        .as_deref()
        .filter(|path| ends_with_ignore_ascii_case(path, ".mks"))
}

/// Whether a stream codec can be copied (rather than transcoded) on extraction.
///
/// Port of `SubtitleEncoder.IsCodecCopyable`.
fn is_codec_copyable(codec: &str) -> bool {
    codec.eq_ignore_ascii_case("ass")
        || codec.eq_ignore_ascii_case("ssa")
        || codec.eq_ignore_ascii_case("srt")
        || codec.eq_ignore_ascii_case("subrip")
        || codec.eq_ignore_ascii_case("pgssub")
        || MediaStream::is_vob_sub_format(Some(codec))
}

/// Returns the lowercased file extension of `path` (without the dot), if any.
fn path_extension(path: &str) -> Option<String> {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
}

/// Replaces `,Arial,` with `,Arial Unicode MS,` in ASS subtitle text.
///
/// Port of the text replacement in `SubtitleEncoder.SetAssFont`: returns
/// `Some(replaced)` when a substitution was made, `None` when the text is
/// unchanged (so the caller can skip the write).
fn set_ass_font_text(text: &str) -> Option<String> {
    let replaced = text.replace(",Arial,", ",Arial Unicode MS,");
    (replaced != *text).then_some(replaced)
}

/// Case-insensitive `str::ends_with`.
fn ends_with_ignore_ascii_case(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack[haystack.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// Case-insensitive `str::starts_with`.
fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;

    use super::{DOTNET_MAX_TIMER_DELAY_MS, subtitle_extraction_timeout};

    /// `TimeSpan.FromMinutes(SubtitleExtractionTimeoutMinutes)`: the stock 30,
    /// any other minute count, and zero (an immediate timeout) are durations;
    /// a negative count, or one past the longest .NET timer delay, is rejected
    /// rather than handed to the wait.
    #[rstest]
    #[case::stock_default(30, Some(Duration::from_mins(30)))]
    #[case::one_minute(1, Some(Duration::from_mins(1)))]
    #[case::zero_is_immediate(0, Some(Duration::ZERO))]
    #[case::negative_is_invalid(-1, None)]
    #[case::most_negative_is_invalid(i32::MIN, None)]
    #[case::largest_is_invalid(i32::MAX, None)]
    fn subtitle_extraction_timeout_is_whole_minutes(
        #[case] minutes: i32,
        #[case] expected: Option<Duration>,
    ) {
        assert_eq!(subtitle_extraction_timeout(minutes), expected);
    }

    /// The upper bound is the last whole minute within .NET's
    /// `Timer.MaxSupportedTimeout` (`UInt32.MaxValue - 1` ms = 71,582 minutes);
    /// one minute more is refused, as upstream's `CancelAfter` would throw.
    #[test]
    fn subtitle_extraction_timeout_stops_at_the_dotnet_timer_limit() {
        let ms_per_minute = Duration::from_mins(1).as_millis();
        let longest = i32::try_from(u128::from(DOTNET_MAX_TIMER_DELAY_MS) / ms_per_minute).unwrap();
        assert_eq!(longest, 71_582);
        assert_eq!(
            subtitle_extraction_timeout(longest),
            Some(Duration::from_mins(71_582))
        );
        assert_eq!(subtitle_extraction_timeout(longest + 1), None);
    }

    #[test]
    fn set_ass_font_text_replaces_arial() {
        let input = "Style: Default,Arial,20,&Hffffff";
        let result = super::set_ass_font_text(input);
        assert_eq!(
            result.as_deref(),
            Some("Style: Default,Arial Unicode MS,20,&Hffffff")
        );
    }

    #[test]
    fn set_ass_font_text_replaces_all_occurrences() {
        let input = "Style: Default,Arial,20\nStyle: Italic,Arial,16";
        let result = super::set_ass_font_text(input).unwrap();
        assert_eq!(result.matches(",Arial Unicode MS,").count(), 2);
        assert_eq!(result.matches(",Arial,").count(), 0);
    }

    #[test]
    fn set_ass_font_text_unchanged_returns_none() {
        assert!(super::set_ass_font_text("Style: Default,Noto Sans,20").is_none());
    }

    #[test]
    fn set_ass_font_text_no_false_positive_on_partial_match() {
        // `,ArialBlack,` should NOT be replaced (only exact `,Arial,`).
        let input = "Style: Default,ArialBlack,20";
        assert!(super::set_ass_font_text(input).is_none());
    }
}
