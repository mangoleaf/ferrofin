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

/// In-memory [`SubtitleIo`] fake: files are served from a byte map; ffmpeg
/// extraction records its argument strings for assertion (the handle is shared
/// so tests keep access after the fake moves into the encoder).
#[derive(Default)]
struct FakeIo {
    files: Mutex<HashMap<String, Vec<u8>>>,
    extract_args: std::sync::Arc<Mutex<Vec<String>>>,
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

    async fn extract(&self, args: &str, _output_paths: &[String]) -> Result<(), String> {
        self.extract_args.lock().unwrap().push(args.to_owned());
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

// ---- GetSubtitleStream BOM-less UTF-16 detection ----------------------------
//
// Mostly-ASCII UTF-16 text without a BOM is also *valid* UTF-8 (NUL bytes are
// legal), so a naive detector serves it verbatim and ♪ (U+266A, UTF-16LE bytes
// 6A 26) renders as the literal text "j&". The C# `UtfUnknown` detector
// classifies these by their null-byte pattern; these tests pin the ported
// behaviour.

fn build_lyric_srt() -> String {
    let mut builder = String::new();
    for i in 1..=8 {
        builder.push_str(&i.to_string());
        builder.push('\n');
        let _ = writeln!(builder, "00:00:0{i},000 --> 00:00:0{},000", i + 1);
        builder.push_str("♪ Come and get your love ♪\n\n");
    }
    builder
}

async fn assert_bomless_utf16_decoded(big_endian: bool) {
    let srt = build_lyric_srt();
    let mut wide = Vec::new();
    for unit in srt.encode_utf16() {
        wide.extend_from_slice(&if big_endian {
            unit.to_be_bytes()
        } else {
            unit.to_le_bytes()
        });
    }

    let path = "/media/lyrics.srt";
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), FakeIo::with_file(path, wide));
    let file_info = SubtitleInfo {
        path: path.to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();
    assert!(stream.is_converted(), "BOM-less UTF-16 must be re-encoded");
    let text = String::from_utf8(stream.into_bytes()).unwrap();
    assert!(text.contains("♪ Come and get your love ♪"));
    assert!(!text.contains("j&"), "music note must not decay to j&");
    assert!(!text.contains('\0'), "no interleaved NULs");
}

#[tokio::test]
async fn get_subtitle_stream_bomless_utf16le_keeps_music_notes() {
    assert_bomless_utf16_decoded(false).await;
}

#[tokio::test]
async fn get_subtitle_stream_bomless_utf16be_keeps_music_notes() {
    assert_bomless_utf16_decoded(true).await;
}

// ---- ConvertTextSubtitleToSrt charset hint ----------------------------------
//
// Port of the C# arg construction: `-y{charenc} -i "{in}" -c:s srt "{out}"`.
// Without `-sub_charenc` ffmpeg decodes a legacy-encoded file as UTF-8 and
// mangles every non-ASCII character; UTF-16 `.smi`/`.sami` must stay unset
// (ffmpeg auto-converts those and rejects an explicit charset).

async fn convert_smi_and_capture_args(bytes: Vec<u8>) -> String {
    let path = "/media/sub.smi";
    let io = FakeIo::with_file(path, bytes);
    let captured = io.extract_args.clone();
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), io);
    let media_source = MediaSourceInfo {
        id: Some("cafe".to_owned()),
        ..MediaSourceInfo::default()
    };
    let stream = external_stream(path);
    encoder
        .get_readable_file(&media_source, &stream)
        .await
        .unwrap();
    let args = captured.lock().unwrap().clone();
    assert_eq!(args.len(), 1, "exactly one ffmpeg conversion expected");
    args.into_iter().next().unwrap()
}

#[tokio::test]
async fn convert_text_subtitle_passes_charenc_for_legacy_encoding() {
    let srt = build_greek_srt();
    let (legacy, _, _) = encoding_rs::WINDOWS_1253.encode(&srt);
    let args = convert_smi_and_capture_args(legacy.into_owned()).await;
    assert!(
        args.contains("-sub_charenc windows-1253"),
        "expected charset hint in: {args}"
    );
}

#[tokio::test]
async fn convert_text_subtitle_omits_charenc_for_utf8() {
    let args = convert_smi_and_capture_args(build_greek_srt().into_bytes()).await;
    assert!(!args.contains("-sub_charenc"), "no charset hint in: {args}");
    assert!(args.starts_with("-y -i"), "single space after -y: {args}");
}

#[tokio::test]
async fn convert_text_subtitle_omits_charenc_for_utf16_smi() {
    let srt = build_greek_srt();
    let mut wide = vec![0xFF, 0xFE];
    for unit in srt.encode_utf16() {
        wide.extend_from_slice(&unit.to_le_bytes());
    }
    let args = convert_smi_and_capture_args(wide).await;
    assert!(
        !args.contains("-sub_charenc"),
        "ffmpeg auto-converts UTF-16 smi; no hint in: {args}"
    );
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
