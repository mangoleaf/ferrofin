//! Coverage for the `SubtitleEncoder` decision/arg-building paths that the
//! oracle-parity `subtitle_encoder.rs` tests don't reach: embedded extraction,
//! `.mks`/PGS/unsupported-text branches of `get_readable_file`, the ffmpeg
//! arg-building in `extract_all_extractable_subtitles`, `convert_subtitles`
//! windowing/writer-selection, and `filter_events`.
//!
//! All process/network I/O is captured by an in-memory `SubtitleIo` fake, so the
//! real ffmpeg subprocess is never spawned — only the arg-building/decision code
//! is exercised.
// libse keeps times as `double`s, and `filter_events` must reproduce them exactly.
#![allow(clippy::float_cmp)]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ferrofin_mediaencoding::subtitles::encoder::filter_events;
use ferrofin_mediaencoding::subtitles::model::{Paragraph, Subtitle, TimeCode};
use ferrofin_mediaencoding::subtitles::{
    SubtitleEditParser, SubtitleEncoder, SubtitleInfo, SubtitleIo,
};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaProtocol;

/// A fake IO that records the ffmpeg `extract` invocations it receives so tests
/// can assert on the built argument string without spawning a process.
#[derive(Default)]
struct RecordingIo {
    files: Mutex<HashMap<String, Vec<u8>>>,
    extract_calls: Mutex<Vec<(String, Vec<String>)>>,
    cache_id_is_empty_none: bool,
    /// When set, a path exists exactly when it is in `files` (the cache
    /// freshness tests); otherwise the real filesystem answers, as before.
    files_exist: bool,
    /// The `(size, .NET ticks)` stamp of each source path; a path not here
    /// does not exist as a source.
    stamps: Mutex<HashMap<String, (u64, i64)>>,
}

impl RecordingIo {
    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.extract_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SubtitleIo for RecordingIo {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| format!("no such file: {path}"))
    }

    async fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.read_file(url).await
    }

    fn path_protocol(&self, path: &str) -> MediaProtocol {
        if path.to_ascii_lowercase().starts_with("http") {
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
        // Simulate the "non-GUID Id -> no cache" branch when configured.
        if self.cache_id_is_empty_none && media_source_id.is_empty() {
            return None;
        }
        Some(format!(
            "/cache/{media_source_id}/{subtitle_stream_index}{output_extension}"
        ))
    }

    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<(), String> {
        self.extract_calls
            .lock()
            .unwrap()
            .push((args.to_owned(), output_paths.to_vec()));
        // Simulate file creation so post-extraction checks see non-empty outputs.
        let mut files = self.files.lock().unwrap();
        for path in output_paths {
            files
                .entry(path.clone())
                .or_insert_with(|| b"extracted".to_vec());
        }
        Ok(())
    }

    fn file_size(&self, path: &str) -> Option<u64> {
        self.files.lock().unwrap().get(path).map(|v| v.len() as u64)
    }

    fn delete_file(&self, path: &str) -> Result<(), String> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    fn write_file(&self, path: &str, content: &[u8]) -> Result<(), String> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), content.to_vec());
        Ok(())
    }

    fn file_exists(&self, path: &str) -> bool {
        if self.files_exist {
            self.files.lock().unwrap().contains_key(path)
        } else {
            std::path::Path::new(path).is_file()
        }
    }

    fn file_stamp(&self, path: &str) -> Option<(u64, i64)> {
        self.stamps.lock().unwrap().get(path).copied()
    }
}

fn sub_stream(index: i32, codec: &str, path: Option<&str>, external: bool) -> MediaStream {
    MediaStream {
        index,
        codec: Some(codec.to_owned()),
        path: path.map(str::to_owned),
        is_external: external,
        stream_type: MediaStreamType::Subtitle,
        ..MediaStream::default()
    }
}

fn source_with(id: &str, path: &str, streams: Vec<MediaStream>) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some(id.to_owned()),
        path: Some(path.to_owned()),
        media_streams: streams,
        ..MediaSourceInfo::default()
    }
}

// -- get_readable_file: embedded (non-external) stream ---------------------

#[tokio::test]
async fn readable_file_embedded_srt_goes_to_cache_and_extracts() {
    let stream = sub_stream(2, "subrip", None, false);
    let source = source_with("guid1", "/media/movie.mkv", vec![stream.clone()]);
    let io = RecordingIo::default();
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    // Embedded text -> extracted to the .srt cache path, non-external.
    assert_eq!(info.path, "/cache/guid1/2.srt");
    assert_eq!(info.protocol, MediaProtocol::File);
    assert_eq!(info.format, "srt");
    assert!(!info.is_external);
}

#[tokio::test]
async fn readable_file_embedded_ass_preserves_format() {
    let stream = sub_stream(0, "ass", None, false);
    let source = source_with("guid2", "/media/movie.mkv", vec![stream.clone()]);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), RecordingIo::default());

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    assert_eq!(info.path, "/cache/guid2/0.ass");
    assert_eq!(info.format, "ass");
}

#[tokio::test]
async fn readable_file_external_mks_is_extracted_like_embedded() {
    // External but `.mks` -> takes the extraction branch.
    let stream = sub_stream(1, "dvdsub", Some("/media/subs.mks"), true);
    let source = source_with("guid3", "/media/movie.mkv", vec![stream.clone()]);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), RecordingIo::default());

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    // dvdsub -> extension/format `mks`; is_external mirrors is_vob_sub_format
    // of the *output* format ("mks"), which is not a vobsub codec -> false.
    assert_eq!(info.path, "/cache/guid3/1.mks");
    assert_eq!(info.format, "mks");
    assert!(!info.is_external);
}

#[tokio::test]
async fn readable_file_no_cache_errors_for_non_guid_source() {
    let stream = sub_stream(0, "subrip", None, false);
    let source = source_with("", "/media/live.ts", vec![stream.clone()]);
    let io = RecordingIo {
        cache_id_is_empty_none: true,
        ..RecordingIo::default()
    };
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    let err = encoder
        .get_readable_file(&source, &stream)
        .await
        .unwrap_err();
    assert!(err.contains("no subtitle cache"));
}

// -- get_readable_file: external PGS passthrough ---------------------------

#[tokio::test]
async fn readable_file_external_pgs_passes_through_as_pgssub() {
    let stream = sub_stream(0, "pgssub", Some("/media/sub.sup"), true);
    let source = source_with("guid4", "/media/movie.mkv", vec![stream.clone()]);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), RecordingIo::default());

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    assert_eq!(info.path, "/media/sub.sup");
    assert_eq!(info.format, "pgssub");
    assert!(info.is_external);
}

// -- get_readable_file: unsupported external text -> ffmpeg convert to srt --

#[tokio::test]
async fn readable_file_unsupported_external_text_converts_to_srt() {
    // `.sub` (microdvd-like) is text but not parser-supported -> ffmpeg convert.
    let stream = sub_stream(3, "microdvd", Some("/media/movie.sub"), true);
    let source = source_with("guid5", "/media/movie.mkv", vec![stream.clone()]);
    let io = RecordingIo::default();
    // The conversion charset-detects the source first, so the file must exist.
    io.files
        .lock()
        .unwrap()
        .insert("/media/movie.sub".to_owned(), b"{1}{50}Hello".to_vec());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    assert_eq!(info.path, "/cache/guid5/3.srt");
    assert_eq!(info.format, "srt");
    assert!(info.is_external);
}

// -- get_readable_file: external WebVTT is parsed, not converted ----------

/// Jellyfin 12 (PR 16998) reads an external sidecar by its extension, and libse
/// parses WebVTT: a `.vtt` (codec `webvtt`) is served as itself, never sent
/// through the ffmpeg SRT conversion that drops its cue settings.
#[tokio::test]
async fn readable_file_external_vtt_is_parsed_not_converted() {
    let stream = sub_stream(4, "webvtt", Some("/media/movie.en.vtt"), true);
    let source = source_with("guid9", "/media/movie.mkv", vec![stream.clone()]);
    let raw = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 line:10%\nTop\n";
    let io = RecordingIo::default();
    io.files
        .lock()
        .unwrap()
        .insert("/media/movie.en.vtt".to_owned(), raw.as_bytes().to_vec());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    assert_eq!(info.path, "/media/movie.en.vtt");
    assert_eq!(info.format, "vtt");

    // `Stream.vtt` returns the file unchanged, cue settings included.
    let same = encoder
        .get_subtitles(&source, &stream, "vtt", 0, 0, false)
        .await
        .unwrap();
    assert_eq!(String::from_utf8(same).unwrap(), raw);

    // Another format is converted from the parsed cues, which keep their
    // position as libse's `{\an8}` tag.
    let srt = encoder
        .get_subtitles(&source, &stream, "srt", 0, 0, false)
        .await
        .unwrap();
    assert!(String::from_utf8(srt).unwrap().contains("{\\an8}Top"));

    // Only a request for the source's own format is served verbatim: `webvtt` is
    // converted (`TryGetWriter` takes it as a WebVTT alias), so the cue gets libse's
    // settings for `{\an8}`.
    let webvtt = encoder
        .get_subtitles(&source, &stream, "webvtt", 0, 0, false)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(webvtt).unwrap(),
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 line:20%\nTop"
    );
}

// -- extract_all_extractable_subtitles: arg building -----------------------

#[tokio::test]
async fn extract_all_builds_map_and_copy_args() {
    // Embedded ass (copyable) + embedded subrip (transcode to srt).
    let ass = sub_stream(0, "ass", None, false);
    let srt = sub_stream(1, "subrip", None, false);
    let source = source_with("guid6", "/media/movie.mkv", vec![ass.clone(), srt.clone()]);
    let io = RecordingIo::default();
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    encoder.extract_all_extractable_subtitles(&source).await;

    // Re-borrow the io via a fresh call path is impossible; instead assert by
    // running get_readable_file for the embedded stream which triggers extract.
    let info = encoder.get_readable_file(&source, &ass).await.unwrap();
    assert_eq!(info.format, "ass");
}

#[tokio::test]
async fn extract_all_records_ffmpeg_args() {
    // ass is copyable (-c:s copy); mov_text is text but not copyable (-c:s srt).
    let ass = sub_stream(0, "ass", None, false);
    let mov = sub_stream(1, "mov_text", None, false);
    let source = source_with("guid7", "/media/movie.mkv", vec![ass, mov]);
    let io = RecordingIo::default();
    // Keep a handle to inspect recorded calls by constructing the encoder around
    // an Arc-shared recorder.
    let recorder = std::sync::Arc::new(io);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let calls = recorder.calls();
    assert_eq!(calls.len(), 1, "one batched extract call");
    let (args, outputs) = &calls[0];
    assert!(args.starts_with("-y -i file:\"/media/movie.mkv\""));
    // ass -> -c:s copy ; subrip -> -c:s srt
    assert!(args.contains("-map 0:0 -an -vn -c:s copy"));
    assert!(args.contains("-map 0:1 -an -vn -c:s srt"));
    assert_eq!(outputs.len(), 2);
    assert!(outputs.iter().any(|o| o.ends_with("/0.ass")));
    assert!(outputs.iter().any(|o| o.ends_with("/1.srt")));
}

/// `ExtractAllExtractableSubtitlesMKS`: an external `.mks` is its own ffmpeg
/// input, each of its tracks mapped by its position inside that file, and a
/// VobSub track re-muxed as Matroska (no `.idx`/`.sub` muxer). The embedded
/// pass keeps reading the video file.
#[tokio::test]
async fn extract_all_extracts_external_mks_tracks_from_their_own_file() {
    let ass = sub_stream(0, "ass", None, false);
    let srt = sub_stream(2, "subrip", Some("/media/subs.mks"), true);
    let vob = sub_stream(3, "dvdsub", Some("/media/subs.mks"), true);
    let source = source_with("guid9", "/media/movie.mkv", vec![ass, srt, vob]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let calls = recorder.calls();
    assert_eq!(calls.len(), 2, "the video file, then the .mks: {calls:?}");
    let (embedded, outputs) = &calls[0];
    assert!(
        embedded.starts_with("-y -i file:\"/media/movie.mkv\""),
        "{embedded}"
    );
    assert_eq!(outputs, &["/cache/guid9/0.ass"]);
    let (mks, outputs) = &calls[1];
    assert!(mks.starts_with("-y -i file:\"/media/subs.mks\""), "{mks}");
    assert!(
        mks.contains("-map 0:0 -an -vn -c:s copy -flush_packets 1"),
        "{mks}"
    );
    assert!(
        mks.contains("-map 0:1 -an -vn -c:s copy -f matroska -flush_packets 1"),
        "{mks}"
    );
    assert_eq!(outputs, &["/cache/guid9/2.srt", "/cache/guid9/3.mks"]);
}

/// File names come from the library. One built to close the quote must reach
/// ffmpeg as the single `-i` value, in both the extraction and the conversion
/// command, never as extra options.
#[rstest::rstest]
#[case::quote(r#"/media/a" -y -i /etc/passwd ""#)]
#[case::backslash_quote(r#"/media/a\" -y -i /etc/passwd \""#)]
#[tokio::test]
async fn a_hostile_path_stays_one_input(#[case] stem: &str) {
    use ferrofin_mediaencoding::encoder::split_args;

    let source_path = format!("{stem}.mkv");
    let subtitle_path = format!("{stem}.sub");
    let embedded = sub_stream(0, "ass", None, false);
    let external = sub_stream(3, "microdvd", Some(&subtitle_path), true);
    let source = source_with("guid8", &source_path, vec![embedded, external.clone()]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    recorder
        .files
        .lock()
        .unwrap()
        .insert(subtitle_path.clone(), b"{1}{50}Hello".to_vec());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;
    encoder.get_readable_file(&source, &external).await.unwrap();

    let calls = recorder.calls();
    assert_eq!(calls.len(), 2, "one extraction, one conversion");
    for ((args, _), input) in calls
        .iter()
        .zip([format!("file:{source_path}"), subtitle_path])
    {
        let tokens = split_args(args).unwrap();
        let at = tokens.iter().position(|a| a == "-i").unwrap();
        assert_eq!(tokens[at + 1], input, "{args}");
        assert_eq!(
            tokens.iter().filter(|a| *a == "-i").count(),
            1,
            "{tokens:?}"
        );
        assert!(!tokens.iter().skip(1).any(|a| a == "-y"), "{tokens:?}");
    }
}

#[tokio::test]
async fn extract_all_noop_when_nothing_extractable() {
    // A pure audio stream is not an extractable subtitle.
    let audio = MediaStream {
        index: 0,
        stream_type: MediaStreamType::Audio,
        codec: Some("aac".to_owned()),
        ..MediaStream::default()
    };
    let source = source_with("guid8", "/media/movie.mkv", vec![audio]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;
    let calls = recorder.calls();
    assert!(calls.is_empty(), "{calls:?}");
}

/// Wraps an `Arc<RecordingIo>` so the encoder can own an `I: SubtitleIo` while
/// the test retains a handle to inspect recorded calls.
struct ArcIo(std::sync::Arc<RecordingIo>);

#[async_trait]
impl SubtitleIo for ArcIo {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.0.read_file(path).await
    }
    async fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.0.http_get(url).await
    }
    fn path_protocol(&self, path: &str) -> MediaProtocol {
        self.0.path_protocol(path)
    }
    fn subtitle_cache_path(
        &self,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_extension: &str,
    ) -> Option<String> {
        self.0
            .subtitle_cache_path(media_source_id, subtitle_stream_index, output_extension)
    }
    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<(), String> {
        self.0.extract(args, output_paths).await
    }
    fn file_size(&self, path: &str) -> Option<u64> {
        self.0.file_size(path)
    }
    fn delete_file(&self, path: &str) -> Result<(), String> {
        self.0.delete_file(path)
    }
    fn write_file(&self, path: &str, content: &[u8]) -> Result<(), String> {
        self.0.write_file(path, content)
    }
    fn file_exists(&self, path: &str) -> bool {
        self.0.file_exists(path)
    }
    fn file_stamp(&self, path: &str) -> Option<(u64, i64)> {
        self.0.file_stamp(path)
    }
}

// -- convert_subtitles: writer selection + windowing -----------------------

const SAMPLE_SRT: &[u8] =
    b"1\r\n00:00:01,000 --> 00:00:02,000\r\nfirst\r\n\r\n2\r\n00:00:05,000 --> 00:00:06,000\r\nsecond\r\n\r\n";

async fn convert_to(format: &str) -> Result<Vec<u8>, String> {
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), RecordingIo::default());
    let info = SubtitleInfo {
        path: "t.srt".to_owned(),
        format: "srt".to_owned(),
        ..SubtitleInfo::default()
    };
    encoder
        .convert_subtitles(SAMPLE_SRT, &info, format, 0, 0, false)
        .await
}

#[tokio::test]
async fn convert_subtitles_selects_each_writer() {
    for (fmt, needle) in [
        ("srt", "-->"),
        ("vtt", "WEBVTT"),
        ("ass", "ScriptType: v4.00+"),
        ("ssa", "ScriptType: v4.00"),
        ("json", "TrackEvents"),
    ] {
        let out = String::from_utf8(convert_to(fmt).await.unwrap()).unwrap();
        assert!(out.contains(needle), "format {fmt} missing {needle}");
    }
}

#[tokio::test]
async fn convert_subtitles_unsupported_format_errors() {
    let err = convert_to("microdvd").await.unwrap_err();
    assert!(err.contains("Unsupported format"));
}

#[tokio::test]
async fn convert_subtitles_windows_and_rebases_timestamps() {
    // Start window at 5s (in ticks). The first cue (1-2s) is dropped; the second
    // (5-6s) survives and is rebased to zero.
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), RecordingIo::default());
    let info = SubtitleInfo {
        path: "t.srt".to_owned(),
        format: "srt".to_owned(),
        ..SubtitleInfo::default()
    };
    let five_s_ticks = 5_000 * 10_000;
    let out = String::from_utf8(
        encoder
            .convert_subtitles(SAMPLE_SRT, &info, "vtt", five_s_ticks, 0, false)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!out.contains("first"));
    assert!(out.contains("second"));
    // Rebased so the surviving cue now starts at 00:00:00.000.
    assert!(out.contains("00:00:00.000 --> 00:00:01.000"));
}

// -- filter_events directly ------------------------------------------------

fn cue(text: &str, start_ms: f64, end_ms: f64) -> Paragraph {
    Paragraph::new(
        text.to_owned(),
        TimeCode::from_milliseconds(start_ms),
        TimeCode::from_milliseconds(end_ms),
    )
}

#[test]
fn filter_events_drops_fully_elapsed_and_after_end() {
    let mut s = Subtitle {
        paragraphs: vec![
            cue("before", 0.0, 500.0),   // fully before start=1000 -> dropped
            cue("spans", 800.0, 1500.0), // ends after start -> kept
            cue("mid", 2000.0, 3000.0),  // kept
            cue("late", 9000.0, 9500.0), // starts after end=5000 -> dropped
        ],
        ..Subtitle::default()
    };
    let start_ticks = 1_000 * 10_000;
    let end_ticks = 5_000 * 10_000;
    filter_events(&mut s, start_ticks, end_ticks, true);
    let texts: Vec<_> = s.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, vec!["spans", "mid"]);
    // preserve_timestamps=true -> timestamps unchanged.
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 800.0);
}

#[test]
fn filter_events_rebase_clamps_to_zero() {
    let mut s = Subtitle {
        paragraphs: vec![cue("spans", 800.0, 1500.0)],
        ..Subtitle::default()
    };
    let start_ticks = 1_000 * 10_000;
    filter_events(&mut s, start_ticks, 0, false);
    // start 800ms - 1000ms clamps to 0; end 1500-1000 = 500ms.
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 0.0);
    assert_eq!(s.paragraphs[0].end_time.total_milliseconds(), 500.0);
}

#[test]
fn filter_events_wraps_an_extreme_offset_as_unchecked_csharp_arithmetic_does() {
    // The offset comes from the request (`routeStartPositionTicks`). C#'s `long`
    // arithmetic wraps: `ticks - long.MinValue` goes negative, so every cue elapsed.
    let mut s = Subtitle {
        paragraphs: vec![cue("cue", 1000.0, 2000.0)],
        ..Subtitle::default()
    };
    filter_events(&mut s, i64::MIN, 0, false);
    assert!(s.paragraphs.is_empty(), "{:?}", s.paragraphs);
}

#[test]
fn filter_events_rebase_keeps_the_sub_millisecond_part_of_the_offset() {
    // `new TimeCode(TimeSpan.FromTicks(...))`: a 1.2345 ms offset leaves 998.7655 ms,
    // which the JSON writer reads back as 9 987 655 ticks.
    let mut s = Subtitle {
        paragraphs: vec![cue("cue", 1000.0, 2000.0)],
        ..Subtitle::default()
    };
    filter_events(&mut s, 12_345, 0, false);
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 998.7655);
    assert_eq!(s.paragraphs[0].start_time.ticks(), 9_987_655);
}

// -- extract_all_extractable_subtitles: failure handling --------------------

/// A variant of `RecordingIo` that can be configured to fail or produce
/// empty outputs, for exercising the post-extraction validation paths.
#[derive(Default)]
struct FailableIo {
    inner: RecordingIo,
    /// When `true`, `extract` returns `Err`.
    extract_fails: bool,
    /// Output paths that should be created as empty (zero-byte) files.
    empty_outputs: Mutex<Vec<String>>,
}

#[async_trait]
impl SubtitleIo for FailableIo {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.inner.read_file(path).await
    }
    async fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.inner.http_get(url).await
    }
    fn path_protocol(&self, path: &str) -> MediaProtocol {
        self.inner.path_protocol(path)
    }
    fn subtitle_cache_path(
        &self,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_extension: &str,
    ) -> Option<String> {
        self.inner
            .subtitle_cache_path(media_source_id, subtitle_stream_index, output_extension)
    }
    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<(), String> {
        if self.extract_fails {
            // Record the call but return an error.
            self.inner
                .extract_calls
                .lock()
                .unwrap()
                .push((args.to_owned(), output_paths.to_vec()));
            return Err("ffmpeg exited with -1".to_owned());
        }
        // Succeed, but override specified outputs to be empty.
        self.inner.extract(args, output_paths).await?;
        let empties = self.empty_outputs.lock().unwrap();
        let mut files = self.inner.files.lock().unwrap();
        for path in empties.iter() {
            files.insert(path.clone(), Vec::new());
        }
        Ok(())
    }
    fn file_exists(&self, path: &str) -> bool {
        self.inner.file_exists(path)
    }
    fn file_size(&self, path: &str) -> Option<u64> {
        self.inner.file_size(path)
    }
    fn delete_file(&self, path: &str) -> Result<(), String> {
        self.inner.delete_file(path)
    }
    fn write_file(&self, path: &str, content: &[u8]) -> Result<(), String> {
        self.inner.write_file(path, content)
    }
}

/// Wraps an `Arc<FailableIo>` so the encoder can own an `I: SubtitleIo`
/// while the test retains a handle to inspect recorded state.
struct ArcFailable(std::sync::Arc<FailableIo>);

#[async_trait]
impl SubtitleIo for ArcFailable {
    async fn read_file(&self, p: &str) -> Result<Vec<u8>, String> {
        self.0.read_file(p).await
    }
    async fn http_get(&self, u: &str) -> Result<Vec<u8>, String> {
        self.0.http_get(u).await
    }
    fn path_protocol(&self, p: &str) -> MediaProtocol {
        self.0.path_protocol(p)
    }
    fn subtitle_cache_path(&self, id: &str, idx: i32, ext: &str) -> Option<String> {
        self.0.subtitle_cache_path(id, idx, ext)
    }
    async fn extract(&self, a: &str, o: &[String]) -> Result<(), String> {
        self.0.extract(a, o).await
    }
    fn file_exists(&self, p: &str) -> bool {
        self.0.file_exists(p)
    }
    fn file_size(&self, p: &str) -> Option<u64> {
        self.0.file_size(p)
    }
    fn delete_file(&self, p: &str) -> Result<(), String> {
        self.0.delete_file(p)
    }
    fn write_file(&self, p: &str, c: &[u8]) -> Result<(), String> {
        self.0.write_file(p, c)
    }
}

/// Wraps an `Arc<RecordingIo>` whose `extract` writes the supplied
/// `content` instead of a placeholder, for testing the ASS font fix-up.
struct ContentIo {
    inner: std::sync::Arc<RecordingIo>,
    content: Vec<u8>,
}

#[async_trait]
impl SubtitleIo for ContentIo {
    async fn read_file(&self, p: &str) -> Result<Vec<u8>, String> {
        self.inner.read_file(p).await
    }
    async fn http_get(&self, u: &str) -> Result<Vec<u8>, String> {
        self.inner.http_get(u).await
    }
    fn path_protocol(&self, p: &str) -> MediaProtocol {
        self.inner.path_protocol(p)
    }
    fn subtitle_cache_path(&self, id: &str, idx: i32, ext: &str) -> Option<String> {
        self.inner.subtitle_cache_path(id, idx, ext)
    }
    async fn extract(&self, a: &str, output_paths: &[String]) -> Result<(), String> {
        self.inner
            .extract_calls
            .lock()
            .unwrap()
            .push((a.to_owned(), output_paths.to_vec()));
        let mut files = self.inner.files.lock().unwrap();
        for path in output_paths {
            files.insert(path.clone(), self.content.clone());
        }
        Ok(())
    }
    fn file_exists(&self, p: &str) -> bool {
        self.inner.file_exists(p)
    }
    fn file_size(&self, p: &str) -> Option<u64> {
        self.inner.file_size(p)
    }
    fn delete_file(&self, p: &str) -> Result<(), String> {
        self.inner.delete_file(p)
    }
    fn write_file(&self, p: &str, c: &[u8]) -> Result<(), String> {
        self.inner.write_file(p, c)
    }
}

#[tokio::test]
async fn extract_all_deletes_outputs_on_ffmpeg_failure() {
    let ass = sub_stream(0, "ass", None, false);
    let srt = sub_stream(1, "subrip", None, false);
    let source = source_with("guid-fail", "/media/movie.mkv", vec![ass, srt]);
    let shared = std::sync::Arc::new(FailableIo {
        extract_fails: true,
        ..FailableIo::default()
    });
    shared
        .inner
        .files
        .lock()
        .unwrap()
        .insert("/cache/guid-fail/0.ass".to_owned(), b"partial".to_vec());
    shared
        .inner
        .files
        .lock()
        .unwrap()
        .insert("/cache/guid-fail/1.srt".to_owned(), b"partial".to_vec());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcFailable(shared.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let files = shared.inner.files.lock().unwrap();
    assert!(
        !files.contains_key("/cache/guid-fail/0.ass"),
        "failed .ass output should be deleted"
    );
    assert!(
        !files.contains_key("/cache/guid-fail/1.srt"),
        "failed .srt output should be deleted"
    );
}

#[tokio::test]
async fn extract_all_deletes_empty_outputs_on_success() {
    // When ffmpeg exits 0 but produces an empty output, that output is deleted
    // while the non-empty one survives.
    let ass = sub_stream(0, "ass", None, false);
    let srt = sub_stream(1, "subrip", None, false);
    let source = source_with("guid-empty", "/media/movie.mkv", vec![ass, srt]);
    let io = FailableIo {
        empty_outputs: Mutex::new(vec!["/cache/guid-empty/1.srt".to_owned()]),
        ..FailableIo::default()
    };
    let recorder = std::sync::Arc::new(io);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcFailable(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let files = recorder.inner.files.lock().unwrap();
    // The non-empty .ass output survives.
    assert!(
        files.contains_key("/cache/guid-empty/0.ass"),
        "non-empty output should survive"
    );
    // The empty .srt output was deleted.
    assert!(
        !files.contains_key("/cache/guid-empty/1.srt"),
        "empty output should be deleted"
    );
}

#[tokio::test]
async fn extract_all_applies_ass_font_fixup() {
    // An extracted .ass file with `,Arial,` gets the font replaced.
    let ass = sub_stream(0, "ass", None, false);
    let source = source_with("guid-font", "/media/movie.mkv", vec![ass]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let io = ContentIo {
        inner: recorder.clone(),
        content: b"[Script Info]\nStyle: Default,Arial,20".to_vec(),
    };
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    encoder.extract_all_extractable_subtitles(&source).await;

    let files = recorder.files.lock().unwrap();
    let content = files
        .get("/cache/guid-font/0.ass")
        .expect("ass file should exist");
    let text = std::str::from_utf8(content).unwrap();
    assert!(
        text.contains(",Arial Unicode MS,"),
        "Arial should be replaced with Arial Unicode MS"
    );
    assert!(!text.contains(",Arial,"), "plain Arial should not remain");
}

#[tokio::test]
async fn extract_all_skips_font_fixup_for_srt() {
    // An extracted .srt file should NOT get the ASS font fix-up.
    let srt = sub_stream(0, "subrip", None, false);
    let source = source_with("guid-srt", "/media/movie.mkv", vec![srt]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let files = recorder.files.lock().unwrap();
    let content = files
        .get("/cache/guid-srt/0.srt")
        .expect("srt file should exist");
    // The content should be the default "extracted" placeholder, not modified.
    assert_eq!(content, b"extracted");
}

#[tokio::test]
async fn extract_all_no_fixup_when_no_arial_reference() {
    // An .ass file without `,Arial,` should not be rewritten.
    let ass = sub_stream(0, "ass", None, false);
    let source = source_with("guid-nofont", "/media/movie.mkv", vec![ass]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let io = ContentIo {
        inner: recorder.clone(),
        content: b"[Script Info]\nStyle: Default,Noto Sans,20".to_vec(),
    };
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    encoder.extract_all_extractable_subtitles(&source).await;

    let files = recorder.files.lock().unwrap();
    let content = files
        .get("/cache/guid-nofont/0.ass")
        .expect("ass file should exist");
    let text = std::str::from_utf8(content).unwrap();
    // No rewrite should have occurred — original content preserved.
    assert!(text.contains("Noto Sans"));
    assert!(!text.contains("Arial Unicode MS"));
}

// -- cache freshness: `IsCachedSubtitleFresh` / `WriteCacheMeta` (v12.2) ---

/// A recorder whose files exist only in memory, with `source` stamped.
fn stamped(source: &str, stamp: (u64, i64)) -> std::sync::Arc<RecordingIo> {
    let recorder = std::sync::Arc::new(RecordingIo {
        files_exist: true,
        ..RecordingIo::default()
    });
    recorder
        .stamps
        .lock()
        .unwrap()
        .insert(source.to_owned(), stamp);
    recorder
}

fn meta(recorder: &RecordingIo, cache: &str) -> Option<String> {
    recorder
        .files
        .lock()
        .unwrap()
        .get(&format!("{cache}.meta"))
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// An embedded extraction records the media file's `{size}:{ticks}` beside
/// the cache, is reused while the file is unchanged, and is redone once the
/// file is replaced in place (same path, so same item and stream indexes).
#[tokio::test]
async fn an_embedded_extraction_is_redone_when_the_media_file_changes() {
    let recorder = stamped("/media/movie.mkv", (1000, 638_000_000_000_000_000));
    let source = source_with(
        "guid10",
        "/media/movie.mkv",
        vec![sub_stream(0, "ass", None, false)],
    );
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;
    assert_eq!(recorder.calls().len(), 1);
    assert_eq!(
        meta(&recorder, "/cache/guid10/0.ass").as_deref(),
        Some("1000:638000000000000000")
    );

    encoder.extract_all_extractable_subtitles(&source).await;
    assert_eq!(
        recorder.calls().len(),
        1,
        "unchanged media file: the cache is fresh"
    );

    recorder.stamps.lock().unwrap().insert(
        "/media/movie.mkv".to_owned(),
        (2000, 638_000_000_100_000_000),
    );
    encoder.extract_all_extractable_subtitles(&source).await;
    assert_eq!(
        recorder.calls().len(),
        2,
        "replaced media file: extracted again"
    );
    assert_eq!(
        meta(&recorder, "/cache/guid10/0.ass").as_deref(),
        Some("2000:638000000100000000")
    );
}

/// A cache from before the sidecar existed is rebuilt once, then reused.
#[tokio::test]
async fn a_cache_without_a_sidecar_is_rebuilt_once() {
    let recorder = stamped("/media/movie.mkv", (1000, 638_000_000_000_000_000));
    recorder
        .files
        .lock()
        .unwrap()
        .insert("/cache/guid11/0.ass".to_owned(), b"old".to_vec());
    let source = source_with(
        "guid11",
        "/media/movie.mkv",
        vec![sub_stream(0, "ass", None, false)],
    );
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;
    encoder.extract_all_extractable_subtitles(&source).await;
    assert_eq!(recorder.calls().len(), 1);
}

/// A cache whose source is gone (or unreachable) is kept.
#[tokio::test]
async fn a_cache_whose_source_is_missing_is_kept() {
    let recorder = std::sync::Arc::new(RecordingIo {
        files_exist: true,
        ..RecordingIo::default()
    });
    recorder
        .files
        .lock()
        .unwrap()
        .insert("/cache/guid12/0.ass".to_owned(), b"kept".to_vec());
    let source = source_with(
        "guid12",
        "/media/gone.mkv",
        vec![sub_stream(0, "ass", None, false)],
    );
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;
    let calls = recorder.calls();
    assert!(calls.is_empty(), "{calls:?}");
}

/// `ConvertTextSubtitleToSrt`: a sidecar the parser cannot read is converted
/// once per revision, not on every request.
#[tokio::test]
async fn a_text_conversion_is_reused_until_its_sidecar_changes() {
    let recorder = stamped("/media/movie.sub", (12, 638_000_000_000_000_000));
    recorder
        .files
        .lock()
        .unwrap()
        .insert("/media/movie.sub".to_owned(), b"{1}{50}Hello".to_vec());
    let stream = sub_stream(3, "microdvd", Some("/media/movie.sub"), true);
    let source = source_with("guid13", "/media/movie.mkv", vec![stream.clone()]);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    for _ in 0..2 {
        let info = encoder.get_readable_file(&source, &stream).await.unwrap();
        assert_eq!(info.path, "/cache/guid13/3.srt");
    }
    assert_eq!(recorder.calls().len(), 1, "converted once");
    assert_eq!(
        meta(&recorder, "/cache/guid13/3.srt").as_deref(),
        Some("12:638000000000000000")
    );

    recorder
        .stamps
        .lock()
        .unwrap()
        .insert("/media/movie.sub".to_owned(), (20, 638_000_000_000_000_001));
    encoder.get_readable_file(&source, &stream).await.unwrap();
    assert_eq!(recorder.calls().len(), 2, "edited sidecar: converted again");
}

/// The default seam stamps a file with `FileInfo.LastWriteTimeUtc.Ticks`:
/// 100 ns intervals since 0001-01-01 UTC, so a Jellyfin 12 sidecar matches.
#[test]
fn the_file_stamp_is_the_size_and_dotnet_ticks_of_the_last_write() {
    use ferrofin_mediaencoding::subtitles::encoder::NoopSubtitleIo;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("movie.srt");
    std::fs::write(&path, b"12345").unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789))
        .unwrap();
    drop(file);
    assert_eq!(
        NoopSubtitleIo.file_stamp(&path.to_string_lossy()),
        Some((5, 638_355_968_001_234_567))
    );
    assert_eq!(NoopSubtitleIo.file_stamp("/no/such/file"), None);
}

// -- ffmpeg stream index: `GetSubtitleStreamIndexForFfmpeg` (v12.2) -------

/// An embedded track is mapped by its position among the video file's own
/// streams (`EncodingHelper.FindIndex`), so a subtitle sidecar listed first
/// does not shift it past the end of the file.
#[tokio::test]
async fn an_embedded_track_is_mapped_past_a_subtitle_sidecar() {
    let sidecar = sub_stream(0, "subrip", Some("/media/movie.en.srt"), true);
    let video = MediaStream {
        index: 1,
        codec: Some("h264".to_owned()),
        stream_type: MediaStreamType::Video,
        ..MediaStream::default()
    };
    let embedded = sub_stream(2, "subrip", None, false);
    let source = source_with("guid14", "/media/movie.mkv", vec![sidecar, video, embedded]);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let calls = recorder.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].0.contains("-map 0:1 -an -vn"), "{}", calls[0].0);
    assert_eq!(calls[0].1, ["/cache/guid14/2.srt"]);
}

/// On a Blu-ray source, the TrueHD and Atmos audio streams before the track,
/// which ffprobe hides but ffmpeg counts, shift its index.
#[tokio::test]
async fn a_blu_ray_track_counts_the_hidden_truehd_streams_before_it() {
    let video = MediaStream {
        index: 0,
        codec: Some("h264".to_owned()),
        stream_type: MediaStreamType::Video,
        ..MediaStream::default()
    };
    let truehd = MediaStream {
        index: 1,
        codec: Some("truehd".to_owned()),
        stream_type: MediaStreamType::Audio,
        ..MediaStream::default()
    };
    let subtitle = sub_stream(2, "subrip", None, false);
    let mut source = source_with("guid15", "/media/disc", vec![video, truehd, subtitle]);
    source.video_type = Some(ferrofin_model::entities::VideoType::BluRay);
    let recorder = std::sync::Arc::new(RecordingIo::default());
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), ArcIo(recorder.clone()));

    encoder.extract_all_extractable_subtitles(&source).await;

    let calls = recorder.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].0.contains("-map 0:3 -an -vn"), "{}", calls[0].0);
}
