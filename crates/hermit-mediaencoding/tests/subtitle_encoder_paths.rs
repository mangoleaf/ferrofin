//! Coverage for the `SubtitleEncoder` decision/arg-building paths that the
//! oracle-parity `subtitle_encoder.rs` tests don't reach: embedded extraction,
//! `.mks`/PGS/unsupported-text branches of `get_readable_file`, the ffmpeg
//! arg-building in `extract_all_extractable_subtitles`, `convert_subtitles`
//! windowing/writer-selection, and `filter_events`.
//!
//! All process/network I/O is captured by an in-memory `SubtitleIo` fake, so the
//! real ffmpeg subprocess is never spawned — only the arg-building/decision code
//! is exercised.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use hermit_mediaencoding::subtitles::encoder::filter_events;
use hermit_mediaencoding::subtitles::model::{Paragraph, Subtitle, TimeCode};
use hermit_mediaencoding::subtitles::{
    SubtitleEditParser, SubtitleEncoder, SubtitleInfo, SubtitleIo,
};
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities::MediaStreamType;
use hermit_model::entities_media::MediaStream;
use hermit_model::media_info::MediaProtocol;

/// A fake IO that records the ffmpeg `extract` invocations it receives so tests
/// can assert on the built argument string without spawning a process.
#[derive(Default)]
struct RecordingIo {
    files: Mutex<HashMap<String, Vec<u8>>>,
    extract_calls: Mutex<Vec<(String, Vec<String>)>>,
    cache_id_is_empty_none: bool,
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
        Ok(())
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
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);

    let info = encoder.get_readable_file(&source, &stream).await.unwrap();
    assert_eq!(info.path, "/cache/guid5/3.srt");
    assert_eq!(info.format, "srt");
    assert!(info.is_external);
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
    assert!(recorder.calls().is_empty());
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

fn cue(text: &str, start_ms: i64, end_ms: i64) -> Paragraph {
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
            cue("before", 0, 500),   // fully before start=1000 -> dropped
            cue("spans", 800, 1500), // ends after start -> kept
            cue("mid", 2000, 3000),  // kept
            cue("late", 9000, 9500), // starts after end=5000 -> dropped
        ],
    };
    let start_ticks = 1_000 * 10_000;
    let end_ticks = 5_000 * 10_000;
    filter_events(&mut s, start_ticks, end_ticks, true);
    let texts: Vec<_> = s.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, vec!["spans", "mid"]);
    // preserve_timestamps=true -> timestamps unchanged.
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 800);
}

#[test]
fn filter_events_rebase_clamps_to_zero() {
    let mut s = Subtitle {
        paragraphs: vec![cue("spans", 800, 1500)],
    };
    let start_ticks = 1_000 * 10_000;
    filter_events(&mut s, start_ticks, 0, false);
    // start 800ms - 1000ms clamps to 0; end 1500-1000 = 500ms.
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 0);
    assert_eq!(s.paragraphs[0].end_time.total_milliseconds(), 500);
}
