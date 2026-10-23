//! Transliteration of `Jellyfin.MediaEncoding.Subtitles.Tests`
//! `SrtParserTests` / `SsaParserTests` / `AssParserTests`. Expected values are
//! the C# oracle verbatim (ticks are `TimeSpan.Parse(...).Ticks`).

use ferrofin_mediaencoding::subtitles::{SubtitleEditParser, SubtitleParser};

/// The `Environment.NewLine` libse joins cue text with — a bare LF on the Linux
/// reference server (the C# oracle is written against `Environment.NewLine`).
const NL: &str = "\n";

fn load(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// `TimeSpan.Parse("HH:MM:SS.fff").Ticks` — 100 ns ticks for a wall-clock time.
fn ticks(h: i64, m: i64, s: i64, millis: i64) -> i64 {
    ((((h * 60) + m) * 60 + s) * 1000 + millis) * 10_000
}

#[test]
fn srt_parse_treats_every_net_line_terminator_alike() {
    // `StreamReader.ReadLine` ends a line at `\r\n`, `\n` or a bare `\r`; the
    // joined cue text is the same whichever the file used.
    let parser = SubtitleEditParser::new();
    for data in [
        "1\r\n00:00:01,000 --> 00:00:02,000\r\nfirst\r\nsecond\r\n",
        "1\n00:00:01,000 --> 00:00:02,000\nfirst\nsecond\n",
        "1\r00:00:01,000 --> 00:00:02,000\rfirst\rsecond\r",
    ] {
        let parsed = parser.parse(data.as_bytes(), "srt").unwrap();
        assert_eq!(parsed.paragraphs.len(), 1, "{data:?}");
        assert_eq!(
            parsed.paragraphs[0].text,
            format!("first{NL}second"),
            "{data:?}"
        );
    }
}

// SrtParserTests.Parse_Valid_Success
#[test]
fn srt_parse_valid_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example.srt"), "srt").unwrap();
    assert_eq!(parsed.paragraphs.len(), 2);

    let p1 = &parsed.paragraphs[0];
    assert_eq!(p1.number, 1);
    assert_eq!(p1.start_time.ticks(), ticks(0, 2, 17, 440));
    assert_eq!(p1.end_time.ticks(), ticks(0, 2, 20, 375));
    assert_eq!(
        p1.text,
        format!("Senator, we're making{NL}our final approach into Coruscant.")
    );

    let p2 = &parsed.paragraphs[1];
    assert_eq!(p2.number, 2);
    assert_eq!(p2.start_time.ticks(), ticks(0, 2, 20, 476));
    assert_eq!(p2.end_time.ticks(), ticks(0, 2, 22, 501));
    assert_eq!(p2.text, "Very good, Lieutenant.");
}

// SrtParserTests.Parse_EmptyNewlineBetweenText_Success
#[test]
fn srt_parse_empty_newline_between_text_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example2.srt"), "srt").unwrap();
    assert_eq!(parsed.paragraphs.len(), 2);

    let p1 = &parsed.paragraphs[0];
    assert_eq!(p1.number, 311);
    assert_eq!(p1.start_time.ticks(), ticks(0, 16, 46, 465));
    assert_eq!(p1.end_time.ticks(), ticks(0, 16, 49, 9));
    assert_eq!(
        p1.text,
        format!("Una vez que la gente se entere{NL}{NL}de que ustedes están aquí,")
    );

    let p2 = &parsed.paragraphs[1];
    assert_eq!(p2.number, 312);
    assert_eq!(p2.start_time.ticks(), ticks(0, 16, 49, 92));
    assert_eq!(p2.end_time.ticks(), ticks(0, 16, 51, 470));
    assert_eq!(
        p2.text,
        format!("este lugar se convertirá{NL}{NL}en un maldito zoológico.")
    );
}

// SsaParserTests.Parse_MultipleDialogues_Success
#[test]
fn ssa_parse_multiple_dialogues_success() {
    let ssa = "[Events]
                Format: Layer, Start, End, Text
                Dialogue: ,0:00:01.18,0:00:01.85,dialogue1
                Dialogue: ,0:00:02.18,0:00:02.85,dialogue2
                Dialogue: ,0:00:03.18,0:00:03.85,dialogue3
                ";

    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(ssa.as_bytes(), "ssa").unwrap();

    let expected: [(&str, &str, i64, i64); 3] = [
        ("1", "dialogue1", 11_800_000, 18_500_000),
        ("2", "dialogue2", 21_800_000, 28_500_000),
        ("3", "dialogue3", 31_800_000, 38_500_000),
    ];
    assert_eq!(parsed.paragraphs.len(), expected.len());

    for (p, (id, text, start, end)) in parsed.paragraphs.iter().zip(expected) {
        assert_eq!(p.number.to_string(), id);
        assert_eq!(p.text, text);
        assert_eq!(p.start_time.ticks(), start);
        assert_eq!(p.end_time.ticks(), end);
    }
}

// SsaParserTests.Parse_Valid_Success
#[test]
fn ssa_parse_valid_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example.ssa"), "ssa").unwrap();
    assert_eq!(parsed.paragraphs.len(), 1);

    let p = &parsed.paragraphs[0];
    assert_eq!(p.number, 1);
    assert_eq!(p.start_time.ticks(), ticks(0, 0, 1, 180));
    assert_eq!(p.end_time.ticks(), ticks(0, 0, 6, 850));
    assert_eq!(p.text, "{\\pos(400,570)}Like an angel with pity on nobody");
}

// AssParserTests.Parse_Valid_Success
#[test]
fn ass_parse_valid_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example.ass"), "ass").unwrap();
    assert_eq!(parsed.paragraphs.len(), 1);

    let p = &parsed.paragraphs[0];
    assert_eq!(p.number, 1);
    assert_eq!(p.start_time.ticks(), ticks(0, 0, 1, 180));
    assert_eq!(p.end_time.ticks(), ticks(0, 0, 6, 850));
    assert_eq!(
        p.text,
        format!(
            "{{\\pos(400,570)}}Like an Angel with pity on nobody{NL}The second line in subtitle"
        )
    );
}

// ---- WebVTT: libse 4.0.12 `WebVTT.LoadSubtitle` (src/Tests/Logic/SubtitleFormats/
// SubtitleFormatsTest.cs is the oracle where it has a case) ----

fn vtt(raw: &str) -> ferrofin_mediaencoding::subtitles::Subtitle {
    SubtitleEditParser::new()
        .parse(raw.as_bytes(), "vtt")
        .expect("parses as WebVTT")
}

// SubtitleFormatsTest.WebVttSpaceBeforeTimeCode
#[test]
fn vtt_space_before_time_code() {
    let parsed = vtt(
        "WEBVTT\n\n 00:39:32.240 --> 00:39:37.640 align:middle\nJag måste ge mig av.\nHemskt ledsen.\n\n00:39:48.960 --> 00:39:51.120 align:middle\n\nVÄLKOMMEN TILL TEXAS\n\n00:40:15.520 --> 00:40:19.640 align:middle\n-Hej, Martin.\n-Hej, pappa.",
    );
    assert_eq!(parsed.paragraphs.len(), 3);
}

// SubtitleFormatsTest.WebVttFontBlankLine
#[test]
fn vtt_blank_line_opens_a_cue() {
    let parsed = vtt(
        "WEBVTT\n\n00:39:32.240 --> 00:39:37.640 align:middle\nJag måste ge mig av.\nHemskt ledsen.\n\n00:39:48.960 --> 00:39:51.120 align:middle\n\nVÄLKOMMEN TILL TEXAS\n\n00:40:15.520 --> 00:40:19.640 align:middle\n-Hej, Martin.\n-Hej, pappa.",
    );
    assert_eq!(
        parsed.paragraphs[1].text,
        format!("{NL}VÄLKOMMEN TILL TEXAS")
    );
    assert_eq!(
        parsed.paragraphs[0].start_time.ticks(),
        ticks(0, 39, 32, 240)
    );
    assert_eq!(parsed.paragraphs[2].end_time.ticks(), ticks(0, 40, 19, 640));
    assert_eq!(parsed.paragraphs[2].number, 3);
}

// SubtitleFormatsTest.WebVttEscapeDecoding
#[test]
fn vtt_escape_decoding() {
    let parsed = vtt(
        "WEBVTT\n\n00:00:00.000 --> 00:00:00.000\n<i>R&amp;D</i>\n\n00:00:00.000 --> 00:00:00.000\ni&lt;5\n\n00:00:00.000 --> 00:00:00.000\ni&gt;6\n\n00:00:00.000 --> 00:00:00.000\n<v Viggo>Hallo\n\n00:00:00.000 --> 00:00:00.000\n&rlm;<c.arabic>مسلسلات NETFLIX ألاصلية</c.arabic>",
    );
    let texts: Vec<&str> = parsed.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "<i>R&D</i>",
            "i<5",
            "i>6",
            "<v Viggo>Hallo",
            "&rlm;<c.arabic>مسلسلات NETFLIX ألاصلية</c.arabic>"
        ]
    );
}

// SubtitleFormatsTest.WebVttXTimestampHeader / WebVttXTimestampHeader2
#[test]
fn vtt_x_timestamp_map_shifts_every_cue() {
    let parsed = vtt(
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n00:00:16.360 --> 00:00:20.840\nLine1\n\n00:00:30.000 --> 00:00:35.000\nLine2",
    );
    assert_eq!(parsed.paragraphs[0].text, "Line1");
    assert_eq!(parsed.paragraphs[0].start_time.total_milliseconds(), 26_360);
    assert_eq!(parsed.paragraphs[0].end_time.total_milliseconds(), 30_840);
    assert_eq!(parsed.paragraphs[1].start_time.total_milliseconds(), 40_000);
    let parsed = vtt(
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:10.000\n\n00:00:16.360 --> 00:00:20.840\nLine1\n\n00:00:30.000 --> 00:00:35.000\nLine2",
    );
    assert_eq!(parsed.paragraphs[0].start_time.total_milliseconds(), 16_360);
    assert_eq!(parsed.paragraphs[1].start_time.total_milliseconds(), 30_000);
}

/// The cue settings survive the load: `GetPositionInfo` turns `line`/`position`
/// into an `{\anN}` tag on the text, `GetPositionInfoRaw` keeps the settings as
/// the paragraph's style and `GetRegion` its region; the header keeps the
/// `STYLE`/`REGION` blocks; minute-only timecodes get their hours.
#[test]
fn vtt_keeps_cue_settings_regions_and_header() {
    let parsed = vtt(
        "WEBVTT - film\n\nSTYLE\n::cue { color: yellow }\n\nREGION\nid:top width:40%\n\nNOTE a comment\n\n1\n00:01.000 --> 00:02.500 region:top line:10% align:start position:10%\nTop left\n\n00:00:03.000 --> 00:00:04.000 line:90% position:90%\nBottom right\n\n00:05.000 --> 00:06.000\nPlain",
    );
    assert_eq!(parsed.paragraphs.len(), 3);
    let first = &parsed.paragraphs[0];
    assert_eq!(first.text, "{\\an7}Top left");
    assert_eq!(first.start_time.total_milliseconds(), 1_000);
    assert_eq!(first.end_time.total_milliseconds(), 2_500);
    assert_eq!(first.style, "line:10% align:start position:10%");
    assert_eq!(first.region.as_deref(), Some("top"));
    assert_eq!(parsed.paragraphs[1].text, "{\\an3}Bottom right");
    assert_eq!(parsed.paragraphs[2].text, "Plain");
    assert_eq!(parsed.paragraphs[2].style, "");
    assert_eq!(parsed.paragraphs[2].region, None);
    assert_eq!(
        parsed.header.as_deref(),
        // `header.Replace(NewLine + NewLine, NewLine).Trim()`, one pass: the
        // blocks were joined by three or four line breaks, so one blank line
        // stays between them.
        Some(
            "WEBVTT - film\n\nSTYLE\n::cue { color: yellow }\n\nREGION\nid:top width:40%\n\nNOTE a comment"
        )
    );
}

/// `SubtitleEditParser.Parse` rejects an extension no format is registered for,
/// and a document no cue could be read from.
#[test]
fn vtt_is_a_supported_extension_and_an_empty_one_is_unsupported() {
    let parser = SubtitleEditParser::new();
    assert!(parser.supports_file_extension("vtt"));
    assert!(parser.supports_file_extension(".VTT"));
    assert!(
        parser
            .parse(b"WEBVTT\n\nNOTE nothing here\n", "vtt")
            .is_err()
    );
}

/// A numbered "WEBVTT FILE" document is libse WebVTT's hand-off to
/// `WebVTTFileWithLineNumber` (errors = cues + 1). That format is not ported
/// yet (gl-768), so these cues are served and must still come out decoded and
/// numbered.
#[test]
fn vtt_numbered_webvtt_file_hand_off_still_decodes_and_numbers() {
    let parsed = vtt(
        "WEBVTT FILE\n\n1\n00:00:01.000 --> 00:00:02.000\nOne &amp; a\n\n2\n00:00:03.000 --> 00:00:04.000\nTwo\n\n3\n00:00:05.000 --> 00:00:06.000\nThree\n\n4\n00:00:07.000 --> 00:00:08.000\nFour",
    );
    let texts: Vec<&str> = parsed.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, ["One & a", "Two", "Three", "Four"]);
    assert_eq!(parsed.paragraphs[3].number, 4);
}

/// `GetXTimeStampSeconds` edge cases: a map after the header shifts the cues
/// below it; a map with no `LOCAL:` still reads `MPEGTS`; a `LOCAL` time that
/// does not parse throws out of the load upstream, and fails the parse here.
#[test]
fn vtt_x_timestamp_map_edges() {
    let later = vtt(
        "WEBVTT\n\nX-TIMESTAMP-MAP=MPEGTS:180000,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:02.000\nShifted",
    );
    assert_eq!(later.paragraphs[0].start_time.total_milliseconds(), 3_000);
    let no_local =
        vtt("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000\n\n00:00:01.000 --> 00:00:02.000\nShifted");
    assert_eq!(
        no_local.paragraphs[0].start_time.total_milliseconds(),
        11_000
    );
    let bad = SubtitleEditParser::new().parse(
        b"WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00::00.000\n\n00:00:01.000 --> 00:00:02.000\nA",
        "vtt",
    );
    assert!(bad.is_err());
}

/// CRLF files read the same. A `STYLE` block after the first cue is no longer
/// header (libse only gathers it before any cue is stored), so it lands in the
/// cue's text, and `RemoveWeirdRepeatingHeader` cuts it off there.
#[test]
fn vtt_crlf_and_a_style_block_leaked_into_a_cue() {
    let parsed = vtt(
        "WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nHello  there\r\n\r\n00:00:03.000 --> 00:00:04.000\r\nB\r\nSTYLE\r\n::cue { color: red }\r\n",
    );
    let texts: Vec<&str> = parsed.paragraphs.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, ["Hello there", "B"]);
    assert_eq!(parsed.header.as_deref(), Some("WEBVTT"));
}
