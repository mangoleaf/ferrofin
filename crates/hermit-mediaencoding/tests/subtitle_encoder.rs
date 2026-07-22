//! Transliteration of `Jellyfin.MediaEncoding.Subtitles.Tests.SubtitleEncoderTests`.
//! Expected values are the C# oracle verbatim.
//!
//! The C# tests wire the encoder up with AutoMoq; here the un-mockable I/O is
//! injected through the [`SubtitleIo`] seam with an in-memory fake, and the real
//! [`SubtitleEditParser`] is used (as `CreateEncoder` injects the concrete
//! `SubtitleEditParser`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use async_trait::async_trait;
use hermit_mediaencoding::subtitles::{
    SubtitleEditParser, SubtitleEncoder, SubtitleInfo, SubtitleIo,
};
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities_media::MediaStream;
use hermit_model::media_info::MediaProtocol;

const STREAM_COUNT: usize = 8;
const CUE_COUNT: usize = 500;

// A Greek line that requires a non-UTF-8 legacy encoding to reproduce the bug.
const GREEK_TEXT: &str = "Καλημέρα κόσμε, αυτό είναι ένας υπότιτλος.";

/// In-memory [`SubtitleIo`] fake: files are served from a byte map; HTTP and
/// ffmpeg extraction are unused by the ported test paths.
#[derive(Default)]
struct FakeIo {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl FakeIo {
    fn with_file(path: &str, bytes: Vec<u8>) -> Self {
        let io = Self::default();
        io.files.lock().unwrap().insert(path.to_owned(), bytes);
        io
    }

    fn path_protocol_of(path: &str) -> MediaProtocol {
        if path.is_empty() {
            MediaProtocol::File
        } else if path.to_ascii_lowercase().starts_with("http") {
            MediaProtocol::Http
        } else {
            MediaProtocol::File
        }
    }
}

#[async_trait]
impl SubtitleIo for FakeIo {
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
        Self::path_protocol_of(path)
    }

    fn subtitle_cache_path(
        &self,
        media_source_id: &str,
        subtitle_stream_index: i32,
        output_extension: &str,
    ) -> Option<String> {
        Some(format!(
            "/cache/{media_source_id}/{subtitle_stream_index}{output_extension}"
        ))
    }

    async fn extract(&self, _args: &str, _output_paths: &[String]) -> Result<(), String> {
        Ok(())
    }
}

fn external_stream(path: &str) -> MediaStream {
    MediaStream {
        path: Some(path.to_owned()),
        is_external: true,
        ..MediaStream::default()
    }
}

// ---- GetReadableFile_Valid_Success (Theory: 4 cases) -----------------------

async fn assert_readable(
    source_protocol: MediaProtocol,
    stream_path: &str,
    expected: SubtitleInfo,
) {
    let media_source = MediaSourceInfo {
        protocol: source_protocol,
        ..MediaSourceInfo::default()
    };
    let stream = external_stream(stream_path);
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), FakeIo::default());
    let result = encoder
        .get_readable_file(&media_source, &stream)
        .await
        .unwrap();
    assert_eq!(result.path, expected.path);
    assert_eq!(result.protocol, expected.protocol);
    assert_eq!(result.format, expected.format);
    assert_eq!(result.is_external, expected.is_external);
}

#[tokio::test]
async fn get_readable_file_valid_ass() {
    assert_readable(
        MediaProtocol::File,
        "/media/sub.ass",
        SubtitleInfo {
            path: "/media/sub.ass".to_owned(),
            protocol: MediaProtocol::File,
            format: "ass".to_owned(),
            is_external: true,
        },
    )
    .await;
}

#[tokio::test]
async fn get_readable_file_valid_ssa() {
    assert_readable(
        MediaProtocol::File,
        "/media/sub.ssa",
        SubtitleInfo {
            path: "/media/sub.ssa".to_owned(),
            protocol: MediaProtocol::File,
            format: "ssa".to_owned(),
            is_external: true,
        },
    )
    .await;
}

#[tokio::test]
async fn get_readable_file_valid_srt() {
    assert_readable(
        MediaProtocol::File,
        "/media/sub.srt",
        SubtitleInfo {
            path: "/media/sub.srt".to_owned(),
            protocol: MediaProtocol::File,
            format: "srt".to_owned(),
            is_external: true,
        },
    )
    .await;
}

#[tokio::test]
async fn get_readable_file_valid_http_source_ass() {
    assert_readable(
        MediaProtocol::Http,
        "/media/sub.ass",
        SubtitleInfo {
            path: "/media/sub.ass".to_owned(),
            protocol: MediaProtocol::File,
            format: "ass".to_owned(),
            is_external: true,
        },
    )
    .await;
}

// ---- GetSubtitleStream charset conversion ----------------------------------

// Enough Greek text to give the charset detector a strong, unambiguous signal.
fn build_greek_srt() -> String {
    let mut builder = String::new();
    for i in 1..=8 {
        builder.push_str(&i.to_string());
        builder.push('\n');
        let _ = writeln!(builder, "00:00:0{i},000 --> 00:00:0{},000", i + 1);
        builder.push_str(GREEK_TEXT);
        builder.push('\n');
        builder.push_str("Η γρήγορη καφέ αλεπού πηδάει πάνω από το τεμπέλικο σκυλί.\n\n");
    }
    builder
}

async fn assert_non_utf8_converted(encoding: &'static encoding_rs::Encoding) {
    let srt = build_greek_srt();
    let (legacy_bytes, _, _) = encoding.encode(&srt);
    let path = "/media/greek.srt";
    let encoder = SubtitleEncoder::new(
        SubtitleEditParser::new(),
        FakeIo::with_file(path, legacy_bytes.into_owned()),
    );
    let file_info = SubtitleInfo {
        path: path.to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();
    let text = String::from_utf8(stream.into_bytes()).unwrap();

    assert!(
        text.contains(GREEK_TEXT),
        "Greek text must survive round-trip"
    );
    assert!(!text.contains('\u{FFFD}'), "no replacement characters");
    assert!(!text.contains('?'), "no '?' fallback characters");
}

#[tokio::test]
async fn get_subtitle_stream_non_utf8_windows_1253() {
    assert_non_utf8_converted(encoding_rs::WINDOWS_1253).await;
}

#[tokio::test]
async fn get_subtitle_stream_non_utf8_iso_8859_7() {
    assert_non_utf8_converted(encoding_rs::ISO_8859_7).await;
}

#[tokio::test]
async fn get_subtitle_stream_non_utf8_utf16le_bom() {
    // Wide encoding with a BOM. encoding_rs has no UTF-16 *encoder*, so emit
    // raw little-endian code units behind a UTF-16LE BOM to exercise the path.
    let srt = build_greek_srt();
    let mut wide = vec![0xFF, 0xFE];
    for unit in srt.encode_utf16() {
        wide.extend_from_slice(&unit.to_le_bytes());
    }

    let path = "/media/greek_utf16.srt";
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), FakeIo::with_file(path, wide));
    let file_info = SubtitleInfo {
        path: path.to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();
    let text = String::from_utf8(stream.into_bytes()).unwrap();

    assert!(text.contains(GREEK_TEXT));
    assert!(!text.contains('\u{FFFD}'));
    assert!(!text.contains('?'));
}

// ---- GetSubtitleStream_Utf8LocalFile_PreservesContent ----------------------

#[tokio::test]
async fn get_subtitle_stream_utf8_local_file_preserves_content() {
    let srt = build_greek_srt();
    let path = "/media/greek_utf8.srt";
    let encoder = SubtitleEncoder::new(
        SubtitleEditParser::new(),
        FakeIo::with_file(path, srt.into_bytes()),
    );
    let file_info = SubtitleInfo {
        path: path.to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

    // An already-UTF-8 file must be short-circuited and served directly (not a
    // re-encoded MemoryStream).
    assert!(!stream.is_converted());

    let text = String::from_utf8(stream.into_bytes()).unwrap();
    assert!(text.contains(GREEK_TEXT));
}

// ---- ConvertSubtitles determinism ------------------------------------------

fn generate_srt(stream_index: usize, cue_count: usize) -> String {
    let mut builder = String::new();
    for i in 0..cue_count {
        let start_s = i * 4;
        let end_s = start_s + 2;
        builder.push_str(&(i + 1).to_string());
        builder.push_str("\r\n");
        let timing = format!(
            "{:02}:{:02}:{:02},{:03} --> {:02}:{:02}:{:02},{:03}",
            start_s / 3600,
            (start_s / 60) % 60,
            start_s % 60,
            0,
            end_s / 3600,
            (end_s / 60) % 60,
            end_s % 60,
            0,
        );
        builder.push_str(&timing);
        builder.push_str("\r\n");
        let cue = format!("S{stream_index}C{i}");
        builder.push_str(&cue);
        builder.push_str("\r\n\r\n");
    }
    builder
}

fn generate_sources() -> Vec<Vec<u8>> {
    (0..STREAM_COUNT)
        .map(|i| generate_srt(i, CUE_COUNT).into_bytes())
        .collect()
}

async fn convert(
    encoder: &SubtitleEncoder<SubtitleEditParser, FakeIo>,
    source: &[u8],
    i: usize,
) -> String {
    let info = SubtitleInfo {
        path: format!("track{i}.srt"),
        format: "srt".to_owned(),
        ..SubtitleInfo::default()
    };
    let out = encoder
        .convert_subtitles(source, &info, "vtt", 0, 0, false)
        .await
        .unwrap();
    String::from_utf8(out).unwrap()
}

async fn convert_all_sequential(
    encoder: &SubtitleEncoder<SubtitleEditParser, FakeIo>,
    sources: &[Vec<u8>],
) -> Vec<String> {
    let mut out = Vec::with_capacity(sources.len());
    for (i, source) in sources.iter().enumerate() {
        out.push(convert(encoder, source, i).await);
    }
    out
}

#[tokio::test]
async fn convert_subtitles_sequential_calls_are_deterministic() {
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), FakeIo::default());
    let sources = generate_sources();

    let first = convert_all_sequential(&encoder, &sources).await;
    let second = convert_all_sequential(&encoder, &sources).await;

    for i in 0..STREAM_COUNT {
        assert!(first[i].contains(&format!("S{i}C{}", CUE_COUNT - 1)));
        assert_eq!(first[i], second[i]);
    }
}

#[tokio::test]
async fn convert_subtitles_concurrent_calls_match_sequential_baseline() {
    const ITERATIONS: usize = 10;

    let encoder = std::sync::Arc::new(SubtitleEncoder::new(
        SubtitleEditParser::new(),
        FakeIo::default(),
    ));
    let sources: std::sync::Arc<Vec<Vec<u8>>> = std::sync::Arc::new(generate_sources());
    let baseline = convert_all_sequential(&encoder, &sources).await;

    for iteration in 0..ITERATIONS {
        let mut handles = Vec::new();
        for i in 0..STREAM_COUNT {
            let enc = encoder.clone();
            let srcs = sources.clone();
            handles.push(tokio::spawn(
                async move { convert(&enc, &srcs[i], i).await },
            ));
        }

        for (i, handle) in handles.into_iter().enumerate() {
            let result = handle.await.unwrap();
            assert_eq!(
                baseline[i], result,
                "Iteration {iteration}: stream {i} returned corrupted content"
            );
        }
    }
}
