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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities_media::MediaStream;
use hermit_model::media_info::{MediaProtocol, subtitle_format};
use tokio::sync::Mutex as AsyncMutex;

use super::model::{Subtitle, TimeCode};
use super::parser::{SubtitleEditParser, SubtitleParser};
use super::{json_writer, srt, ssa, vtt};

/// The output format a [`GetWriter`](write)-selected writer emits.
///
/// Port of the `TryGetWriter` switch in the C# encoder, restricted to the
/// formats the writers support (`ass`, `ssa`, `srt`/`subrip`, `vtt`/`webvtt`,
/// `json`).
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
}

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
        } else {
            None
        }
    }

    /// Writes `subtitle` in this writer's format.
    fn write(self, subtitle: &Subtitle) -> String {
        match self {
            Self::Ass => ssa::to_text_ass(subtitle),
            Self::Ssa => ssa::to_text_ssa(subtitle),
            Self::SubRip => srt::to_text(subtitle),
            Self::WebVtt => vtt::to_text(subtitle),
            Self::Json => json_writer::to_text(subtitle),
        }
    }
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
    /// Returns an error message if the file cannot be read.
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;

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
    /// Port of `RunSubtitleExtractionProcess`.
    ///
    /// # Errors
    ///
    /// Returns an error message on ffmpeg failure.
    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<(), String>;
}

/// A set of keyed async mutexes, replacing the C# `AsyncKeyedLocker<string>`.
///
/// Guarantees that operations sharing a key (an output cache path, or a
/// conversion stream key) never run concurrently, preserving the determinism the
/// `SubtitleEncoder` relies on.
#[derive(Default)]
struct KeyedLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl KeyedLocks {
    /// Returns the mutex for `key`, creating it on first use.
    fn get(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self.locks.lock().expect("keyed-lock map is not poisoned");
        map.entry(key.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
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
    /// over `[start, end]`. The caller resolves the [`MediaSourceInfo`] and the
    /// target subtitle stream from the item + media-source id.
    ///
    /// # Errors
    ///
    /// Returns an error message when the readable file cannot be resolved/read,
    /// or when `output_format` has no writer.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_subtitles(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
        output_format: &str,
        start_time_ticks: i64,
        end_time_ticks: i64,
        preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, String> {
        let info = self
            .get_readable_file(media_source, subtitle_stream)
            .await?;
        let stream = self.get_subtitle_stream(&info).await?;

        // Return the original when the same format is requested. ASS is a
        // superset of SSA, so an SSA source satisfies an ASS request verbatim
        // (styles preserved). Character encoding was handled in
        // `get_subtitle_stream`.
        if info.format.eq_ignore_ascii_case(output_format)
            || (info.format.eq_ignore_ascii_case(subtitle_format::SSA)
                && output_format.eq_ignore_ascii_case(subtitle_format::ASS))
        {
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
    /// Returns an error message if parsing fails or `output_format` has no writer.
    pub async fn convert_subtitles(
        &self,
        data: &[u8],
        input_info: &SubtitleInfo,
        output_format: &str,
        start_time_ticks: i64,
        end_time_ticks: i64,
        preserve_original_timestamps: bool,
    ) -> Result<Vec<u8>, String> {
        let guard = self.locks.get(&input_info.path);
        let _held = guard.lock().await;

        let mut subtitle = self.parser.parse(data, &input_info.format)?;

        filter_events(
            &mut subtitle,
            start_time_ticks,
            end_time_ticks,
            preserve_original_timestamps,
        );

        let writer = Writer::for_format(output_format)
            .ok_or_else(|| format!("Unsupported format: {output_format}"))?;

        Ok(writer.write(&subtitle).into_bytes())
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
    /// Returns an error message if the underlying read fails.
    pub async fn get_subtitle_stream(
        &self,
        file_info: &SubtitleInfo,
    ) -> Result<SubtitleStream, String> {
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
            let bytes = self.io.read_file(&file_info.path).await?;
            Ok(SubtitleStream::new(bytes, false))
        }
    }

    /// Resolves the on-disk (or extracted) path of a subtitle stream.
    ///
    /// Port of `SubtitleEncoder.GetSubtitleFilePath`: the readable-file path
    /// (extraction runs first when the stream is embedded/`.mks`).
    ///
    /// # Errors
    ///
    /// Returns an error message when the readable file cannot be resolved.
    pub async fn get_subtitle_file_path(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
    ) -> Result<String, String> {
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
    /// Returns an error message when the subtitle bytes cannot be read.
    pub async fn get_subtitle_file_character_set(
        &self,
        subtitle_stream: &MediaStream,
    ) -> Result<String, String> {
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
    async fn read_bytes(&self, path: &str, protocol: MediaProtocol) -> Result<Vec<u8>, String> {
        if protocol == MediaProtocol::Http {
            self.io.http_get(path).await
        } else {
            self.io.read_file(path).await
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
    /// Returns an error message when the media source has no subtitle cache or
    /// ffmpeg conversion fails.
    pub async fn get_readable_file(
        &self,
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
    ) -> Result<SubtitleInfo, String> {
        let stream_path = subtitle_stream.path.clone().unwrap_or_default();

        if !subtitle_stream.is_external || ends_with_ignore_ascii_case(&stream_path, ".mks") {
            self.extract_all_extractable_subtitles(media_source).await;

            let output_file_extension = get_extractable_subtitle_file_extension(subtitle_stream);
            let output_format = get_extractable_subtitle_format(subtitle_stream);
            let output_path = self
                .subtitle_cache_path(media_source, subtitle_stream.index, &output_file_extension)
                .ok_or_else(|| {
                    format!(
                        "MediaSource {} has no subtitle cache (non-GUID Id, e.g. Live TV stream).",
                        media_source.id.clone().unwrap_or_default()
                    )
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
                    format!(
                        "MediaSource {} has no subtitle cache (non-GUID Id, e.g. Live TV stream).",
                        media_source.id.clone().unwrap_or_default()
                    )
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
    /// Port of `SubtitleEncoder.ExtractAllExtractableSubtitles`, reduced to the
    /// embedded (non-`.mks`) extraction path; each output cache path is
    /// serialized with the keyed lock. Errors are swallowed (logged in C#) so a
    /// single un-extractable stream cannot abort the caller.
    pub async fn extract_all_extractable_subtitles(&self, media_source: &MediaSourceInfo) {
        let mut extractable: Vec<&MediaStream> = Vec::new();
        for stream in &media_source.media_streams {
            if !stream.is_extractable_subtitle_stream() {
                continue;
            }
            let stream_path = stream.path.clone().unwrap_or_default();
            if stream.is_external && !ends_with_ignore_ascii_case(&stream_path, ".mks") {
                continue;
            }
            extractable.push(stream);
        }

        if extractable.is_empty() {
            return;
        }

        let source_path = media_source.path.clone().unwrap_or_default();
        let input_path = Self::io_input_argument(&source_path, media_source);
        let mut args = format!("-y -i {input_path}");
        let mut output_paths: Vec<String> = Vec::new();

        for stream in extractable {
            let stream_path = stream.path.clone().unwrap_or_default();
            if !stream_path.is_empty() && ends_with_ignore_ascii_case(&stream_path, ".mks") {
                continue;
            }
            let ext = get_extractable_subtitle_file_extension(stream);
            let Some(output_path) = self.subtitle_cache_path(media_source, stream.index, &ext)
            else {
                continue;
            };
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
            let Some(stream_index) = find_index(&media_source.media_streams, stream) else {
                continue;
            };
            output_paths.push(output_path.clone());
            let _ = write!(
                args,
                " -map 0:{stream_index} -an -vn -c:s {output_codec}{output_format_option} -flush_packets 1 \"{output_path}\""
            );
        }

        if output_paths.is_empty() {
            return;
        }

        // Serialize per output path so concurrent extractions of the same cache
        // never race, matching the C# per-path `AsyncKeyedLock`.
        let mut guards = Vec::new();
        for path in &output_paths {
            guards.push(self.locks.get(path));
        }
        let mut held = Vec::new();
        for g in &guards {
            held.push(g.lock().await);
        }

        let _ = self.io.extract(&args, &output_paths).await;
    }

    /// Converts an unsupported text subtitle to SRT via ffmpeg, once per path.
    ///
    /// Port of `ConvertTextSubtitleToSrt`: guarded by the keyed lock so a given
    /// output path is only converted by one caller at a time.
    async fn convert_text_subtitle_to_srt(
        &self,
        subtitle_stream: &MediaStream,
        output_path: &str,
    ) -> Result<(), String> {
        let guard = self.locks.get(output_path);
        let _held = guard.lock().await;

        let input_path = subtitle_stream.path.clone().unwrap_or_default();
        if input_path.is_empty() {
            return Err("inputPath is empty".to_owned());
        }
        let args = format!("-y  -i \"{input_path}\" -c:s srt \"{output_path}\"");
        self.io
            .extract(&args, std::slice::from_ref(&output_path.to_owned()))
            .await
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

    /// Builds the ffmpeg `-i` input argument for a source path.
    ///
    /// The C# code calls `IMediaEncoder.GetInputArgument`; here the un-ported
    /// hardware-aware builder is reduced to the file/URL quoting the extraction
    /// arg-building needs (the full builder lives in the `encoder` module).
    fn io_input_argument(path: &str, _media_source: &MediaSourceInfo) -> String {
        if path.contains("://") {
            format!("\"{path}\"")
        } else {
            format!("file:\"{}\"", path.replace('"', "\\\""))
        }
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
    async fn read_file(&self, _path: &str) -> Result<Vec<u8>, String> {
        Err("no subtitle I/O configured".to_owned())
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

    async fn extract(&self, _args: &str, _output_paths: &[String]) -> Result<(), String> {
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
        !((p.start_time.ticks() - start_position_ticks) < 0
            && (p.end_time.ticks() - start_position_ticks) < 0)
    });

    if end_time_ticks > 0 {
        track
            .paragraphs
            .retain(|p| p.start_time.ticks() <= end_time_ticks);
    }

    if !preserve_timestamps {
        for p in &mut track.paragraphs {
            p.start_time =
                TimeCode::from_ticks((p.start_time.ticks() - start_position_ticks).max(0));
            p.end_time = TimeCode::from_ticks((p.end_time.ticks() - start_position_ticks).max(0));
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
/// A UTF-8/UTF-16 BOM or valid-UTF-8 content short-circuits to UTF-8; otherwise
/// [`chardetng`] guesses the legacy encoding. UTF-16 (with BOM) decodes through
/// `encoding_rs` so wide files round-trip.
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
    if std::str::from_utf8(bytes).is_ok() {
        return DetectedCharset::Utf8OrAscii;
    }

    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, true);
    if encoding == encoding_rs::UTF_8 {
        DetectedCharset::Utf8OrAscii
    } else {
        DetectedCharset::Other(encoding)
    }
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

/// Finds the ordinal index of `stream` within `streams` by identity/index.
///
/// Port of `EncodingHelper.FindIndex(mediaStreams, stream)`, matched on the
/// stream `index` (its stable identity in a media source).
fn find_index(streams: &[MediaStream], stream: &MediaStream) -> Option<i32> {
    streams
        .iter()
        .position(|s| s.index == stream.index)
        .and_then(|p| i32::try_from(p).ok())
}

/// Returns the lowercased file extension of `path` (without the dot), if any.
fn path_extension(path: &str) -> Option<String> {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
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
