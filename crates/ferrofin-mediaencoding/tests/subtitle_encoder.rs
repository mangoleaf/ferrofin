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
use ferrofin_mediaencoding::subtitles::{
    SubtitleEditParser, SubtitleEncoder, SubtitleInfo, SubtitleIo,
};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaProtocol;

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
}

#[async_trait]
impl SubtitleIo for FakeIo {
    async fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }

    async fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.read_file(url).await.map_err(|e| e.to_string())
    }

    fn path_protocol(&self, path: &str) -> MediaProtocol {
        ferrofin_model::media_info::path_protocol(path)
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

    async fn extract(&self, args: &str, output_paths: &[String]) -> Result<String, String> {
        self.extract_args.lock().unwrap().push(args.to_owned());
        // Simulate file creation so post-extraction checks see non-empty outputs.
        let mut files = self.files.lock().unwrap();
        for path in output_paths {
            files
                .entry(path.clone())
                .or_insert_with(|| b"extracted".to_vec());
        }
        Ok(String::new())
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

// ---- GetSubtitleStream ISO-2022-JP detection --------------------------------
//
// ISO-2022-JP is 7-bit: every byte is ASCII and ESC sequences switch the
// character set, so the file is also valid UTF-8. `UtfUnknown`'s escape prober
// detects it before any UTF-8 answer, and so must the port (gl-1422).

const JAPANESE_TEXT: &str = "日本語の字幕です。";

fn encoder_for_srt(
    path: &str,
    bytes: Vec<u8>,
) -> (SubtitleEncoder<SubtitleEditParser, FakeIo>, SubtitleInfo) {
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), FakeIo::with_file(path, bytes));
    let file_info = SubtitleInfo {
        path: path.to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    (encoder, file_info)
}

#[tokio::test]
async fn get_subtitle_stream_iso_2022_jp_is_converted() {
    let srt = format!("1\n00:00:01,000 --> 00:00:02,000\n{JAPANESE_TEXT}\n\n");
    let (jis, _, _) = encoding_rs::ISO_2022_JP.encode(&srt);
    assert!(jis.is_ascii(), "the fixture is 7-bit");
    let (encoder, file_info) = encoder_for_srt("/media/japanese.srt", jis.into_owned());

    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

    assert!(stream.is_converted(), "ISO-2022-JP must be re-encoded");
    let text = String::from_utf8(stream.into_bytes()).unwrap();
    assert!(text.contains(JAPANESE_TEXT), "{text:?}");
    assert!(
        !text.contains('\u{1B}'),
        "no escape sequences left: {text:?}"
    );
}

#[tokio::test]
async fn get_subtitle_stream_utf8_with_or_without_an_escape_is_preserved() {
    // Plain ASCII stays on the shortcut; `ESC [` is no ISO-2022-JP designation,
    // and non-ASCII UTF-8 is never ISO-2022-JP, so the detector answers UTF-8.
    for cue in [
        "red".to_owned(),
        "\u{1B}[31mred\u{1B}[0m".to_owned(),
        format!("\u{1B}[31m{JAPANESE_TEXT}\u{1B}[0m"),
    ] {
        let srt = format!("1\n00:00:01,000 --> 00:00:02,000\n{cue}\n\n");
        let (encoder, file_info) = encoder_for_srt("/media/utf8.srt", srt.clone().into_bytes());

        let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

        assert!(!stream.is_converted(), "{cue:?}");
        assert_eq!(stream.bytes(), srt.as_bytes());
    }
}

// ---- 7-bit escape encodings (gl-1657) ---------------------------------------
//
// `UtfUnknown`'s escape prober detects ISO-2022-KR and HZ-GB-2312 besides
// ISO-2022-JP, and Jellyfin decodes both through .NET's code pages. The
// fixtures are what Python's `iso2022_kr` and `hz` codecs write.

/// `한국어 자막입니다.`, then an ASCII cue with a tilde, as ISO-2022-KR.
const ISO_2022_KR_SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,500\n\x1b$)C\x0eGQ19>n\x0f \x0e@Z87@T4O4Y\x0f.\n\n2\n00:00:02,600 --> 00:00:02,900\nASCII ~~ tilde line\n\n";

/// `中文字幕。`, then the same ASCII cue, as HZ-GB-2312, where `~~` is one `~`.
const HZ_GB_2312_SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,500\n~{VPNDWVD;!#~}\n\n2\n00:00:02,600 --> 00:00:02,900\nASCII ~~~~ tilde line\n\n";

#[rstest::rstest]
#[case::iso_2022_kr(ISO_2022_KR_SRT, "한국어 자막입니다.")]
#[case::hz_gb_2312(HZ_GB_2312_SRT, "中文字幕。")]
#[tokio::test]
async fn get_subtitle_stream_converts_a_seven_bit_escape_encoding(
    #[case] bytes: &[u8],
    #[case] cue: &str,
) {
    let (encoder, file_info) = encoder_for_srt("/media/escape.srt", bytes.to_vec());

    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

    assert!(stream.is_converted(), "{cue}");
    assert_eq!(
        String::from_utf8(stream.into_bytes()).unwrap(),
        format!(
            "1\n00:00:01,000 --> 00:00:02,500\n{cue}\n\n2\n00:00:02,600 --> 00:00:02,900\nASCII ~~ tilde line\n\n"
        )
    );
}

#[rstest::rstest]
#[case::iso_2022_kr(ISO_2022_KR_SRT, "iso-2022-kr")]
#[case::hz_gb_2312(HZ_GB_2312_SRT, "hz-gb-2312")]
#[tokio::test]
async fn convert_text_subtitle_passes_charenc_for_a_seven_bit_escape_encoding(
    #[case] bytes: &[u8],
    #[case] charset: &str,
) {
    let args = convert_smi_and_capture_args(bytes.to_vec()).await;
    assert!(
        args.contains(&format!("-sub_charenc {charset}")),
        "expected charset hint in: {args}"
    );
}

// ---- What .NET's code pages hold that encoding_rs lacks (gl-2000) ------------
//
// Jellyfin decodes Korean text with .NET's code page 949, alone (`cp949`) or
// under ISO-2022-KR (code page 50225). Its rows C9 and FE are the Private Use
// Area from U+E000, 94 to a row, which WHATWG's EUC-KR index leaves out.
// Measured on Jellyfin 12.4: a sidecar holding every pair of both rows is
// served as U+E000 to U+E0BB, in both forms.

/// Enough Korean that chardetng keeps EUC-KR beside two user-defined pairs,
/// which it counts against the charset.
const KOREAN_TEXT: &str = "이 영화는 정말 재미있습니다. 우리는 내일 다시 만날 것입니다. 그는 집에 가고 싶어 합니다. 안녕하세요, 만나서 반갑습니다.";

#[tokio::test]
async fn get_subtitle_stream_reads_the_user_defined_rows_of_code_page_949() {
    let text = KOREAN_TEXT.repeat(4);
    let (korean, _, _) = encoding_rs::EUC_KR.encode(&text);
    // C9A1 and FEFE, the first and the last user-defined cell.
    let uhc = [
        b"1\n00:00:01,000 --> 00:00:02,500\n",
        &korean[..],
        b" \xc9\xa1 \xfe\xfe\n\n",
    ]
    .concat();
    let iso_2022_kr =
        b"1\n00:00:01,000 --> 00:00:02,500\n\x1b$)C\x0eGQ19>n\x0f \x0eI!\x0f \x0e~~\x0f\n\n"
            .to_vec();

    for (bytes, cue) in [(uhc, text.as_str()), (iso_2022_kr, "한국어")] {
        let (encoder, file_info) = encoder_for_srt("/media/korean.srt", bytes);

        let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

        assert_eq!(
            String::from_utf8(stream.into_bytes()).unwrap(),
            format!("1\n00:00:01,000 --> 00:00:02,500\n{cue} \u{E000} \u{E0BB}\n\n")
        );
    }
}

// Jellyfin reads 8-bit Simplified Chinese as `gb18030`, and .NET's GB18030
// decodes the four-byte sequence 81 35 F4 37 to U+1E3F, where WHATWG's index
// has U+E7C7. Measured on Jellyfin 12.4 (gl-2000).
#[tokio::test]
async fn get_subtitle_stream_reads_the_four_byte_m_acute_of_gb18030() {
    let text = "这是中文字幕。我们明天再见。";
    let (chinese, _, _) = encoding_rs::GBK.encode(text);
    let srt = [
        b"1\n00:00:01,000 --> 00:00:02,500\n",
        &chinese[..],
        b" \x81\x35\xf4\x37\n\n",
    ]
    .concat();
    let (encoder, file_info) = encoder_for_srt("/media/chinese.srt", srt);

    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

    assert_eq!(
        String::from_utf8(stream.into_bytes()).unwrap(),
        format!("1\n00:00:01,000 --> 00:00:02,500\n{text} \u{1E3F}\n\n")
    );
}

// Bytes with no character in their charset are what .NET's decoders write
// for them, as Jellyfin 12.4 serves them (gl-2010): `?`, U+30FB in the
// Japanese code pages, and a code point of its own for a lone byte. Each
// sequence is one chardetng keeps the charset beside, at a price the text
// before it has to outweigh.
#[rstest::rstest]
#[case::cp949(encoding_rs::EUC_KR, KOREAN_TEXT, b" \x80 \xff", " \u{80} \u{F8F7}")]
#[case::gb18030(encoding_rs::GBK, "这是中文字幕。我们明天再见。", b" \xff", " ?")]
#[case::big5(encoding_rs::BIG5, "這是中文字幕。我們明天再見。", b" \xa3\xe2", " ?")]
#[case::shift_jis(
    encoding_rs::SHIFT_JIS,
    "これは日本語の字幕です。あなたはどこにいますか。",
    b" \x81\xad \xa0",
    " \u{30FB} \u{F8F0}"
)]
#[case::euc_jp(
    encoding_rs::EUC_JP,
    "これは日本語の字幕です。あなたはどこにいますか。",
    b" \xa9\xa1",
    " \u{30FB}"
)]
#[tokio::test]
async fn get_subtitle_stream_reads_bytes_with_no_character_as_dotnet_does(
    #[case] encoding: &'static encoding_rs::Encoding,
    #[case] text: &str,
    #[case] bytes: &[u8],
    #[case] expected: &str,
) {
    let text = text.repeat(40);
    let (encoded, _, _) = encoding.encode(&text);
    let srt = [
        b"1\n00:00:01,000 --> 00:00:02,500\n",
        &encoded[..],
        bytes,
        b"\n\n",
    ]
    .concat();
    let (encoder, file_info) = encoder_for_srt("/media/legacy.srt", srt);

    let stream = encoder.get_subtitle_stream(&file_info).await.unwrap();

    assert_eq!(
        String::from_utf8(stream.into_bytes()).unwrap(),
        format!("1\n00:00:01,000 --> 00:00:02,500\n{text}{expected}\n\n")
    );
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
async fn convert_text_subtitle_passes_charenc_for_iso_2022_jp() {
    let smi = format!("<SAMI><BODY><SYNC Start=1000><P>{JAPANESE_TEXT}</BODY></SAMI>\n");
    let (jis, _, _) = encoding_rs::ISO_2022_JP.encode(&smi);
    let args = convert_smi_and_capture_args(jis.into_owned()).await;
    assert!(
        args.contains("-sub_charenc iso-2022-jp"),
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
    Vec::from_fn(STREAM_COUNT, |i| generate_srt(i, CUE_COUNT).into_bytes())
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
