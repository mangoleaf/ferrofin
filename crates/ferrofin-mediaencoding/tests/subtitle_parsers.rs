//! Transliteration of `Jellyfin.MediaEncoding.Subtitles.Tests`
//! `SrtParserTests` / `SsaParserTests` / `AssParserTests`, and of libse 4.0.12's
//! own SubRip and WebVTT loader tests. Expected values are the C# oracle verbatim
//! (ticks are `TimeSpan.Parse(...).Ticks`).
// libse keeps times as `double`s; the WebVTT cases below are exact in one.
#![allow(clippy::float_cmp)]

use ferrofin_mediaencoding::subtitles::libse_text::split_to_lines;
use ferrofin_mediaencoding::subtitles::srt;
use ferrofin_mediaencoding::subtitles::{Subtitle, SubtitleEditParser, SubtitleParser};
use rstest::rstest;

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
    assert_eq!(
        parsed.paragraphs[0].start_time.total_milliseconds(),
        26_360.0
    );
    assert_eq!(parsed.paragraphs[0].end_time.total_milliseconds(), 30_840.0);
    assert_eq!(
        parsed.paragraphs[1].start_time.total_milliseconds(),
        40_000.0
    );
    let parsed = vtt(
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:10.000\n\n00:00:16.360 --> 00:00:20.840\nLine1\n\n00:00:30.000 --> 00:00:35.000\nLine2",
    );
    assert_eq!(
        parsed.paragraphs[0].start_time.total_milliseconds(),
        16_360.0
    );
    assert_eq!(
        parsed.paragraphs[1].start_time.total_milliseconds(),
        30_000.0
    );
}

/// libse adds the `X-TIMESTAMP-MAP` shift to its `double` milliseconds
/// (`TotalMilliseconds += addSeconds * 1000`), so a shift that is not a whole
/// number of milliseconds is kept, and the writers read it back through
/// `TimeSpan.FromMilliseconds`, which truncates.
#[test]
fn vtt_x_timestamp_map_keeps_a_sub_millisecond_shift() {
    let parsed = vtt(
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900009,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:02.000\nA",
    );
    let shift = 900_009.0 / 90_000.0 * 1000.0;
    let cue = &parsed.paragraphs[0];
    assert_eq!(cue.start_time.total_milliseconds(), 1_000.0 + shift);
    assert_eq!(cue.end_time.total_milliseconds(), 2_000.0 + shift);
    assert_eq!(cue.start_time.ticks(), 110_001_000);
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
    assert_eq!(first.start_time.total_milliseconds(), 1_000.0);
    assert_eq!(first.end_time.total_milliseconds(), 2_500.0);
    assert_eq!(
        first.style.as_deref(),
        Some("line:10% align:start position:10%")
    );
    assert_eq!(first.region.as_deref(), Some("top"));
    assert_eq!(parsed.paragraphs[1].text, "{\\an3}Bottom right");
    assert_eq!(parsed.paragraphs[2].text, "Plain");
    assert_eq!(parsed.paragraphs[2].style.as_deref(), Some(""));
    assert_eq!(parsed.paragraphs[2].region, None);
    assert_eq!(
        parsed.header,
        // `header.Replace(NewLine + NewLine, NewLine).Trim()`, one pass: the
        // blocks were joined by three or four line breaks, so one blank line
        // stays between them.
        "WEBVTT - film\n\nSTYLE\n::cue { color: yellow }\n\nREGION\nid:top width:40%\n\nNOTE a comment"
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
    assert_eq!(later.paragraphs[0].start_time.total_milliseconds(), 3_000.0);
    let no_local =
        vtt("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000\n\n00:00:01.000 --> 00:00:02.000\nShifted");
    assert_eq!(
        no_local.paragraphs[0].start_time.total_milliseconds(),
        11_000.0
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
    assert_eq!(parsed.header, "WEBVTT");
}

// ---- SubRip: libse 4.0.12 `SubRip.LoadSubtitle` (src/Tests/Logic/SubtitleFormats/
// SubtitleFormatsTest.cs, its "SubRip (.srt)" region, is the oracle) ----

/// `new SubRip().LoadSubtitle(subtitle, GetSrtLines(text), null)`; libse's tests
/// split with `StringExtensions.SplitToLines`.
fn srt_lines(text: &str) -> Subtitle {
    let mut subtitle = Subtitle::new();
    srt::load(&mut subtitle, &split_to_lines(text));
    subtitle
}

/// [`srt_lines`]'s `Errors`.
fn srt_errors(text: &str) -> String {
    srt::load(&mut Subtitle::new(), &split_to_lines(text)).1
}

fn texts(subtitle: &Subtitle) -> Vec<&str> {
    subtitle
        .paragraphs
        .iter()
        .map(|p| p.text.as_str())
        .collect()
}

// SubtitleFormatsTest.SrtCoordinates
#[test]
fn srt_coordinates_after_the_time_codes_are_cut() {
    let parsed = srt_lines(
        "1\n00:00:02,001 --> 00:00:16,001  X1:000 X2:000 Y1:050 Y2:100\nLet us have some! Let us have some!",
    );
    assert_eq!(
        parsed.paragraphs[0].text,
        "Let us have some! Let us have some!"
    );
}

// SubtitleFormatsTest.SrtNoLineNumbers
#[test]
fn srt_no_line_numbers() {
    let parsed = srt_lines(
        "00:00:03,000 --> 00:00:08,000\nLine1.\n\n00:00:08,000 --> 00:00:09,920\nLine 2.",
    );
    assert_eq!(parsed.paragraphs.len(), 2);
}

// SubtitleFormatsTest.SrtDotsInsteadOfCommas
#[test]
fn srt_dots_instead_of_commas() {
    let parsed = srt_lines("2\n00:00:04.501 --> 00:00:08.500\nDots instead of commas");
    assert_eq!(parsed.paragraphs[0].text, "Dots instead of commas");
}

// SubtitleFormatsTest.SrtTwoLiner / SrtThreeLiner
#[rstest]
#[case::two("2\n00:00:04.501 --> 00:00:08.500\nLine 1\nLine 2", "Line 1\nLine 2")]
#[case::three(
    "2\n00:00:04.501 --> 00:00:08.500\nLine 1\nLine 2\nLine 3",
    "Line 1\nLine 2\nLine 3"
)]
fn srt_multi_line_cues(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(srt_lines(text).paragraphs[0].text, expected);
}

// SubtitleFormatsTest.SrtZeroWidthWhitepaceCharInTimeStamp
#[test]
fn srt_zero_width_spaces_in_a_time_code_line() {
    let parsed = srt_lines(
        "1\n01:05:58,455 --> 01:05:59,260\nPapa? Papa?\n\n2\n01:06:01,725 \u{200b}\u{200b}--> 01:06:03,805\nEs ist einfach, Sie mitzunehmen.",
    );
    assert_eq!(parsed.paragraphs.len(), 2);
    assert_eq!(parsed.paragraphs[0].text, "Papa? Papa?");
}

// SubtitleFormatsTest.SrtKeepBlankLines
#[test]
fn srt_keeps_blank_lines_through_a_round_trip() {
    let sub_text = format!("Now go on!{NL}{NL}Now go on!");
    let mut subtitle = Subtitle::new();
    subtitle
        .paragraphs
        .push(ferrofin_mediaencoding::subtitles::Paragraph::new(
            sub_text.clone(),
            ferrofin_mediaencoding::subtitles::TimeCode::from_milliseconds(0.0),
            ferrofin_mediaencoding::subtitles::TimeCode::from_milliseconds(999.0),
        ));
    let text = srt::to_text(&subtitle);
    let out = srt_lines(&text);
    assert_eq!(out.paragraphs[0].text, sub_text);
}

// SubtitleFormatsTest.SrtDifficultLines1
#[test]
fn srt_two_blank_lines_inside_a_cue() {
    let parsed = srt_lines(
        "303\n00:16:22,417 --> 00:16:24,417\nsky bots?\n\n304\n00:16:24,417 --> 00:16:27,042\nhow do you think i did\n\n\nall the stuff\nfor the show?\n\n305\n00:16:27,042 --> 00:16:29,417\nyou think i went myself?",
    );
    assert_eq!(parsed.paragraphs.len(), 3);
    assert_eq!(
        parsed.paragraphs[1].text,
        "how do you think i did\n\n\nall the stuff\nfor the show?"
    );
}

// SubtitleFormatsTest.SrtLineNumbers / SrtSpacesPre
#[rstest]
#[case::trailing_spaces(
    "1\n00:01:10,040 --> 00:01:13,360 \nK-A-R...\n\n2\n00:01:16,840 --> 00:01:19,080 \n... A-N-T...\n\n3\n00:01:21,280 --> 00:01:23,720 \n... Æ-N-E.\n\n4\n00:01:27,120 --> 00:01:29,680 \nKarantæne.\n\n5\n00:01:29,840 --> 00:01:32,360 \nK-A-R..."
)]
#[case::leading_spaces(
    "1\n 00:01:10,040 --> 00:01:13,360 \nK-A-R...\n\n 2\n00:01:16,840 --> 00:01:19,080 \n... A-N-T...\n\n3\n 00:01:21,280 --> 00:01:23,720 \n... Æ-N-E.\n\n4\n00:01:27,120 --> 00:01:29,680 \nKarantæne.\n\n5\n00:01:29,840 --> 00:01:32,360 \nK-A-R..."
)]
fn srt_spaces_around_numbers_and_time_codes(#[case] text: &str) {
    let parsed = srt_lines(text);
    assert_eq!(parsed.paragraphs.len(), 5);
    assert_eq!(parsed.paragraphs[0].text, "K-A-R...");
}

// SubtitleFormatsTest.SrtDifficultLines2
#[test]
fn srt_a_number_inside_a_cue_after_a_blank_line() {
    let parsed = srt_lines(
        "1\n01:38:18,534 --> 01:38:20,067\n \n6530\n\n2\n01:39:17,534 --> 01:39:19,400\nppp\n",
    );
    assert_eq!(parsed.paragraphs.len(), 2);
    assert_eq!(parsed.paragraphs[0].text.trim(), "6530");
}

// SubtitleFormatsTest.SrtBadTimeCode1
#[test]
fn srt_a_number_as_the_whole_cue_text() {
    let parsed = srt_lines(
        "1\n00:00:16.583 --> 00:00:19.833\n1941\n\n2\n00:00:21.333 --> 00:00:24.083\nHalf the world is at war...\n\n3\n00:00:26.500 --> 00:00:28.875\nGermany has taken most of Europe...",
    );
    assert_eq!(
        texts(&parsed),
        [
            "1941",
            "Half the world is at war...",
            "Germany has taken most of Europe..."
        ]
    );
}

// SubtitleFormatsTest.SrtMissingNewLinesAndBadTimeCodes
#[test]
fn srt_missing_blank_lines_and_spaced_time_codes() {
    let parsed = srt_lines(
        "1\n00: 00: 12,083  -->  00: 00: 15,726\ntext1\n 2\n00: 00: 15,750  -->  00: 00: 28,892\ntext2\n3\n00: 00: 28,916  -->  00: 00: 33,726\ntext3a\ntext3b",
    );
    assert_eq!(texts(&parsed), ["text1", "text2", "text3a\ntext3b"]);
}

// SubtitleFormatsTest.MissingLineNumberAndEmptyLines
#[test]
fn srt_missing_line_numbers_and_an_empty_cue() {
    let parsed = srt_lines(
        "00:00:00,000 --> 00:00:10,000\nabc\n\n00:00:11,000 --> 00:00:20,000\n\n00:00:21,000 --> 00:00:30,000\ndef",
    );
    assert_eq!(texts(&parsed), ["abc", "", "def"]);
    assert_eq!(parsed.paragraphs[2].end_time.total_seconds(), 30.0);
}

// SubtitleFormatsTest.TestBadFileWithMissingNewLineAndBadNumbers.
#[test]
fn srt_missing_blank_lines_and_numbers_that_do_not_follow() {
    let text = "\n00:21:29,998 --> 00:21:32,960\nLONDYN 8 maja 1945.\n\nJeszcze 13 lat temu,\n\n-Kwadrans po 12... Mamo.\n67\n\n00:21:58,986 --> 00:22:01,947\nGŁÓWNY\n012 235 4526\nmodel 5732.\n93\n00:22:03,991 --> 00:22:05,784\n- Za wasz\n- Za wasz\n115\n00:22:18,931 --> 00:22:20,674\n- How do you do, Mr. Van Cleve?\n- How do you do, Mr. Van Cleve?\n\n124\n\n00:22:21,175 --> 00:22:23,121\n10:15 PM.\nThat's it, you're staying in bed.\n\n312\n00:24:21,175 --> 00:24:23,121\nREŻYSERIA\n611\n00:27:21,175 --> 00:27:23,121\nZA ŁATWO, PRAGNIE MŁODOŚCI";
    let parsed = srt_lines(text);
    assert_eq!(
        texts(&parsed),
        [
            "LONDYN 8 maja 1945.\n\nJeszcze 13 lat temu,\n\n-Kwadrans po 12... Mamo.",
            "GŁÓWNY\n012 235 4526\nmodel 5732.",
            "- Za wasz\n- Za wasz",
            "- How do you do, Mr. Van Cleve?\n- How do you do, Mr. Van Cleve?",
            "10:15 PM.\nThat's it, you're staying in bed.",
            "REŻYSERIA",
            "ZA ŁATWO, PRAGNIE MŁODOŚCI",
        ]
    );
    let times: Vec<(String, String)> = parsed
        .paragraphs
        .iter()
        .map(|p| (p.start_time.to_srt_string(), p.end_time.to_srt_string()))
        .collect();
    for (i, start, end) in [
        (0, "00:21:29,998", "00:21:32,960"),
        (1, "00:21:58,986", "00:22:01,947"),
        (2, "00:22:03,991", "00:22:05,784"),
        (6, "00:27:21,175", "00:27:23,121"),
    ] {
        assert_eq!(times[i], (start.to_owned(), end.to_owned()), "cue {i}");
    }
    let errors = srt_errors(text);
    for line in ["Line 8: ", "Line 14: ", "Line 18: ", "Line 32: "] {
        assert!(errors.contains(line), "{line} in {errors}");
    }
}

// ---- SubRip repairs and the frames reading, as libse 4.0.12 loads them through
// Jellyfin's `SubtitleEditParser.Parse` (gl-757). Times are in milliseconds,
// before `FilterEvents`. ----

fn srt_bytes(text: &str) -> Subtitle {
    SubtitleEditParser::new()
        .parse(text.as_bytes(), "srt")
        .expect("parses as SubRip")
}

fn cue_times(subtitle: &Subtitle) -> Vec<(f64, f64)> {
    subtitle
        .paragraphs
        .iter()
        .map(|p| {
            (
                p.start_time.total_milliseconds(),
                p.end_time.total_milliseconds(),
            )
        })
        .collect()
}

#[rstest]
#[case::two_digit_fractions_are_frames("00:00:01,12 --> 00:00:02,20", 1_500.0, 2_834.0)]
#[case::frames_past_a_second_stop_at_999_ms("00:00:01,24 --> 00:00:02,30", 1_999.0, 2_999.0)]
// The `Milliseconds` setter keeps a negative time's sign on every part: -1 s and
// -500 ms (the -12 frames).
#[case::frames_on_a_negative_start("-00:00:01,12 --> 00:00:02,20", -1_500.0, 2_834.0)]
// Past 99:59:59.999 the `TimeSpan` reads as zero, so the setter rebuilds zero.
#[case::frames_past_the_largest_time("100:00:00,12 --> 100:00:01,20", 0.0, 0.0)]
#[case::a_single_dash_arrow("00:00:01,000 -> 00:00:02,000", 1_000.0, 2_000.0)]
#[case::an_em_dash_arrow("00:00:01,000 —> 00:00:02,000", 1_000.0, 2_000.0)]
#[case::a_spaced_arrow("00:00:01,000 - > 00:00:02,000", 1_000.0, 2_000.0)]
#[case::a_double_headed_arrow("00:00:01,000 ->> 00:00:02,000", 1_000.0, 2_000.0)]
#[case::a_long_arrow("00:00:01,000 ---> 00:00:02,000", 1_000.0, 2_000.0)]
#[case::no_hours("01:02,500 --> 01:04,000", 62_500.0, 64_000.0)]
#[case::dots_between_every_field("00.00.02,000 --> 00.00.04,000", 2_000.0, 4_000.0)]
#[case::dots_before_the_milliseconds("00:00:02.000 --> 00:00:04.000", 2_000.0, 4_000.0)]
#[case::a_negative_start("-00:00:01,000 --> 00:00:02,000", -1_000.0, 2_000.0)]
#[case::a_negative_end("00:00:01,000 --> -00:00:02,000", 1_000.0, -2_000.0)]
#[case::dvd_rip_position_info(
    "00:00:01,000 --> 00:00:02,000  X1:63 X2:223 Y1:43 Y2:58",
    1_000.0,
    2_000.0
)]
#[case::position_info_after_one_space(
    "00:00:01,000 --> 00:00:02,000 X1:100 X2:100 Y1:100 Y2:100",
    1_000.0,
    2_000.0
)]
#[case::an_arabic_comma("00:00:01\u{60c}000 --> 00:00:02\u{60c}000", 1_000.0, 2_000.0)]
#[case::spaces_after_the_colons("00: 00: 01,000 --> 00: 00: 02,000", 1_000.0, 2_000.0)]
fn srt_time_code_line_repairs(#[case] timing: &str, #[case] start: f64, #[case] end: f64) {
    let parsed = srt_bytes(&format!("1\n{timing}\ntext\n"));
    assert_eq!(cue_times(&parsed), [(start, end)], "{timing}");
    assert_eq!(parsed.paragraphs[0].text, "text");
}

/// `_isMsFrames` is one flag for the whole file: a single fraction with three
/// digits, or a two-digit one past 30, makes every fraction milliseconds.
#[rstest]
#[case::a_three_digit_fraction_in_a_later_cue(
    "1\n00:00:01,12 --> 00:00:02,20\na\n\n2\n00:00:03,000 --> 00:00:04,000\nb\n",
    [(1_012.0, 2_020.0), (3_000.0, 4_000.0)]
)]
#[case::a_two_digit_fraction_past_30(
    "1\n00:00:01,12 --> 00:00:02,20\na\n\n2\n00:00:03,10 --> 00:00:04,45\nb\n",
    [(1_012.0, 2_020.0), (3_010.0, 4_045.0)]
)]
#[case::every_fraction_frames(
    "1\n00:00:01,12 --> 00:00:02,20\na\n\n2\n00:00:03,00 --> 00:00:04,01\nb\n",
    [(1_500.0, 2_834.0), (3_000.0, 4_042.0)]
)]
fn srt_frames_are_read_only_when_every_fraction_can_be_one(
    #[case] text: &str,
    #[case] expected: [(f64, f64); 2],
) {
    assert_eq!(cue_times(&srt_bytes(text)), expected);
}

#[rstest]
#[case::a_nul_is_a_space("nul\0char", "nul char")]
#[case::trailing_white_space_on_each_line("Hello   \nworld\t\nend\u{a0}", "Hello\nworld\nend")]
#[case::del_around_a_line("\u{7f}deleted\u{7f}", "deleted")]
#[case::white_space_before_a_trailing_del("kept \u{7f}", "kept")]
fn srt_cue_text_repairs(#[case] text: &str, #[case] expected: &str) {
    let parsed = srt_bytes(&format!("1\n00:00:01,000 --> 00:00:02,000\n{text}\n"));
    assert_eq!(parsed.paragraphs[0].text, expected);
}

/// A cue without its number line makes libse renumber every cue from 1.
#[test]
fn srt_a_missing_number_line_renumbers_every_cue() {
    let parsed = srt_bytes(
        "10\n00:00:01,000 --> 00:00:02,000\nten\n\n00:00:03,000 --> 00:00:04,000\nno number\n\n30\n00:00:05,000 --> 00:00:06,000\nthirty\n",
    );
    let numbers: Vec<i32> = parsed.paragraphs.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [1, 2, 3]);
    assert_eq!(texts(&parsed), ["ten", "no number", "thirty"]);
}

/// With every number line in place libse keeps the file's own numbers.
#[test]
fn srt_numbered_cues_keep_their_numbers() {
    let parsed = srt_bytes(
        "10\n00:00:01,000 --> 00:00:02,000\nten\n\n30\n00:00:05,000 --> 00:00:06,000\nthirty\n",
    );
    let numbers: Vec<i32> = parsed.paragraphs.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [10, 30]);
}

/// A line that is neither a number nor a time code where libse expects a cue
/// number joins the previous cue when the line after next is that cue's number
/// plus one; otherwise it is an error and is dropped, and the cues around it
/// still load. White-space-only lines before it end the cue, so it is read where
/// a number is expected.
#[rstest]
#[case::joins_the_previous_cue("2", ["one\nstray", "two"], 0, "")]
#[case::is_an_error_when_the_next_number_does_not_follow(
    "3",
    ["one", "two"],
    1,
    "Line 7 - expected subtitle number: stray\n"
)]
fn srt_a_stray_line_between_cues(
    #[case] next_number: &str,
    #[case] expected: [&str; 2],
    #[case] errors: i32,
    #[case] messages: &str,
) {
    let text = format!(
        "1\n00:00:01,000 --> 00:00:02,000\none\n\n \n \nstray\n\n{next_number}\n00:00:03,000 --> 00:00:04,000\ntwo\n"
    );
    let mut subtitle = Subtitle::new();
    assert_eq!(
        srt::load(&mut subtitle, &split_to_lines(&text)),
        (errors, messages.to_owned())
    );
    assert_eq!(texts(&subtitle), expected);
}

/// The timing line `ReadLine` expects is read with `validate`: a field out of range
/// is reported with the repaired line but not counted, and the cue still loads; a
/// line that is no time code is reported and counted, and the text after it is then
/// read where a cue number is expected, another counted error.
#[rstest]
#[case::a_fraction_over_999(
    "00:00:01,000 -> 00:00:02,1000",
    0,
    "Line 2 - error reading time code: 00:00:01,000 --> 00:00:02,1000\n"
)]
#[case::minutes_over_99(
    "00:100:01,000 --> 00:00:02,000",
    0,
    "Line 2 - error reading time code: 00:100:01,000 --> 00:00:02,000\n"
)]
#[case::no_time_code(
    "soon",
    2,
    "Line 2 - error reading time code: soon\nLine 3 - expected subtitle number: text\n"
)]
fn srt_a_timing_line_libse_reports(
    #[case] timing: &str,
    #[case] errors: i32,
    #[case] messages: &str,
) {
    let text = format!("1\n{timing}\ntext\n");
    assert_eq!(
        srt::load(&mut Subtitle::new(), &split_to_lines(&text)),
        (errors, messages.to_owned())
    );
}

/// libse stops collecting messages once they reach 2000 UTF-16 units; the count
/// goes on. Each emoji is two units, four bytes and one `char`: a cap counted in
/// bytes or in `char`s stops at another message than libse's.
#[test]
fn srt_messages_stop_at_2000_units_but_the_count_does_not() {
    let text = "1\nnot a time code \u{1f600}\u{1f600}\n\n".repeat(200);
    let (errors, messages) = srt::load(&mut Subtitle::new(), &split_to_lines(&text));
    assert_eq!(errors, 200);
    let length = messages.encode_utf16().count();
    let last = messages.lines().last().unwrap().encode_utf16().count() + 1;
    assert!(length >= 2000 && length - last < 2000, "{length}");
}

// ---- the parse log carries libse's `Errors` (gl-1218) ----

/// The text logged through it while its subscriber is the default.
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// The events section both SubStation loaders read: one good event, then one
/// whose start is no time code (line 4).
const SSA_WITH_A_BAD_EVENT: &str = "[Events]\nFormat: Layer, Start, End, Text\nDialogue: 0,0:00:01.00,0:00:02.00,one\nDialogue: 0,notatime,0:00:03.50,x\n";

/// A format that loads with errors logs libse's messages beside the count, as
/// upstream's `SubtitleEditParser.Parse` logs `Errors` (`TryGetErrors`), so the
/// log names the line. At debug: it repeats on every request for the same file.
#[rstest]
#[case::subrip(
    "srt",
    "1\nnot a time code\n\n2\n00:00:03,000 --> 00:00:04,000\ntwo\n",
    "Line 2 - error reading time code: not a time code"
)]
#[case::ssa(
    "ssa",
    SSA_WITH_A_BAD_EVENT,
    "Line 4 - error reading time code: Dialogue: 0,notatime,0:00:03.50,x"
)]
#[case::ass(
    "ass",
    SSA_WITH_A_BAD_EVENT,
    "Line 4 - error reading time code: Dialogue: 0,notatime,0:00:03.50,x"
)]
fn a_load_with_errors_logs_libses_messages(
    #[case] extension: &str,
    #[case] data: &str,
    #[case] message: &str,
) {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );
    SubtitleEditParser::new()
        .parse(data.as_bytes(), extension)
        .unwrap();

    let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    let event = text
        .lines()
        .find(|line| line.contains("errors encountered while parsing a subtitle"))
        .unwrap_or_else(|| panic!("no error event in {text}"));
    assert!(event.contains(" DEBUG "), "{event}");
    assert!(event.contains("errors=1"), "{event}");
    assert!(event.contains(message), "{event}");
}
