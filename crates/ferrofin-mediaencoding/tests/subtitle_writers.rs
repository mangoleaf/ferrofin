//! Coverage for the libse 4.0.12 subtitle writers and SSA/ASS loaders, the
//! extension-keyed parser edge cases, and the `TimeCode`/`Paragraph` model.
//!
//! The expected values are libse's own output for the same input (run through
//! Jellyfin 12.2's conversion path on .NET 10); `subtitle_libse_conversions.rs`
//! checks whole files byte for byte, these pin the individual rules.
// libse keeps times as `double`s, and the port must reproduce them exactly.
#![allow(clippy::float_cmp)]

use ferrofin_mediaencoding::subtitles::model::{
    Paragraph, Subtitle, TICKS_PER_MILLISECOND, TimeCode,
};
use ferrofin_mediaencoding::subtitles::{SubtitleEditParser, SubtitleParser};
use ferrofin_mediaencoding::subtitles::{json_writer, srt, ssa, ssa_tags, vtt};
use rstest::rstest;

fn subtitle_with(cues: &[(&str, f64, f64)]) -> Subtitle {
    let mut s = Subtitle::new();
    for (text, start_ms, end_ms) in cues {
        s.paragraphs.push(Paragraph::new(
            (*text).to_owned(),
            TimeCode::from_milliseconds(*start_ms),
            TimeCode::from_milliseconds(*end_ms),
        ));
    }
    s.renumber();
    s
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

// -- TimeCode / model ------------------------------------------------------

#[test]
fn timecode_tick_roundtrip() {
    let tc = TimeCode::from_milliseconds(1_234.0);
    assert_eq!(tc.total_milliseconds(), 1_234.0);
    assert_eq!(tc.ticks(), 1_234 * TICKS_PER_MILLISECOND);
    // `TimeSpan.FromTicks(...).TotalMilliseconds` keeps the sub-millisecond part.
    let back = TimeCode::from_ticks(12_345_678);
    assert_eq!(back.total_milliseconds(), 1_234.567_8);
    assert_eq!(back.ticks(), 12_345_678);
}

#[rstest]
#[case::truncates_a_tick_and_a_half(0.00015, 1)]
#[case::truncates_two_and_a_half(0.00025, 2)]
#[case::truncates_toward_zero(-0.00015, -1)]
#[case::beyond_99_59_59_999_is_zero(360_000_000.0, 0)]
fn timecode_reads_back_through_timespan_from_milliseconds(#[case] ms: f64, #[case] ticks: i64) {
    assert_eq!(TimeCode::from_milliseconds(ms).ticks(), ticks);
}

#[test]
fn timecode_components_do_not_wrap_the_hours() {
    let tc = TimeCode::from_hms(25, 1, 2, 345);
    assert_eq!(
        (tc.hours(), tc.minutes(), tc.seconds(), tc.milliseconds()),
        (25, 1, 2, 345)
    );
    assert_eq!(tc.to_srt_string(), "25:01:02,345");
}

#[test]
fn timecode_to_string_prefixes_one_sign_for_a_negative_time() {
    assert_eq!(
        TimeCode::from_milliseconds(-5_000.0).to_srt_string(),
        "-00:00:05,000"
    );
}

#[test]
fn renumber_is_one_based_and_sequential() {
    let s = subtitle_with(&[("a", 0.0, 100.0), ("b", 200.0, 300.0), ("c", 400.0, 500.0)]);
    assert_eq!(
        s.paragraphs.iter().map(|p| p.number).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

// -- SubRip writer ---------------------------------------------------------

#[test]
fn srt_to_text_is_subrips_to_text() {
    // No BOM; the cue's own number; the whole trimmed, then two line ends.
    let mut s = subtitle_with(&[
        ("Hello", 137_440.0, 140_375.0),
        ("x", 3_661_500.0, 3_662_000.0),
    ]);
    s.paragraphs[1].number = 7;
    assert_eq!(
        srt::to_text(&s),
        "1\n00:02:17,440 --> 00:02:20,375\nHello\n\n7\n01:01:01,500 --> 01:01:02,000\nx\n\n"
    );
}

#[test]
fn srt_to_text_keeps_the_text_as_it_is() {
    let s = subtitle_with(&[("{\\an8}<i>one</i>\\ntwo", 0.0, 1_000.0)]);
    assert!(srt::to_text(&s).contains("\n{\\an8}<i>one</i>\\ntwo\n"));
}

#[test]
fn srt_to_text_of_nothing_is_two_line_ends() {
    assert_eq!(srt::to_text(&Subtitle::new()), "\n\n");
}

// -- WebVTT writer ---------------------------------------------------------

#[test]
fn vtt_to_text_has_no_bom_region_or_trailing_line_end() {
    let s = subtitle_with(&[("Hi", 61_500.0, 62_250.0), ("Bye", 63_000.0, 63_000.0)]);
    assert_eq!(
        vtt::to_text(&s),
        "WEBVTT\n\n00:01:01.500 --> 00:01:02.250\nHi\n\n00:01:03.000 --> 00:01:03.000\nBye"
    );
}

#[test]
fn vtt_empty_subtitle_is_the_signature_only() {
    assert_eq!(vtt::to_text(&Subtitle::new()), "WEBVTT");
}

#[rstest]
#[case(1, " position:20%")]
#[case(2, "")]
#[case(3, " position:80%")]
#[case(4, " position:20% line:50%")]
#[case(5, " line:50%")]
#[case(6, " position:80% line:50%")]
#[case(7, " position:20% line:20%")]
#[case(8, " line:20%")]
#[case(9, " position:80% line:20%")]
fn vtt_places_an_aligned_cue_with_libses_cue_settings(#[case] an: u8, #[case] settings: &str) {
    let s = subtitle_with(&[(&format!("{{\\an{an}}}Sign"), 1_000.0, 2_000.0)]);
    assert_eq!(
        vtt::to_text(&s),
        format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000{settings}\nSign")
    );
}

#[test]
fn vtt_strips_override_tags_and_collapses_blank_lines() {
    let s = subtitle_with(&[(
        "{\\pos(1,2)}{\\fad(10,10)}one\n\n\ntwo\\Nthree\\hfour",
        0.0,
        1_000.0,
    )]);
    assert!(
        vtt::to_text(&s).ends_with("\none\ntwo\nthree four"),
        "{}",
        vtt::to_text(&s)
    );
}

#[test]
fn vtt_escapes_text_but_keeps_the_tags_webvtt_knows() {
    let s = subtitle_with(&[(
        "<i>a</i> & b < c > d &amp; <b>e</b> &nbsp; <odd>",
        0.0,
        1_000.0,
    )]);
    assert!(
        // `&nbsp;` has four letters, and libse only keeps a reference of two or three.
        vtt::to_text(&s)
            .ends_with("\n<i>a</i> &amp; b &lt; c &gt; d &amp; <b>e</b> &amp;nbsp; &lt;odd&gt;"),
        "{}",
        vtt::to_text(&s)
    );
}

#[rstest]
#[case::lower_case_name(r#"<font color="red">x</font>"#, "<c.red>x</c>")]
#[case::unquoted_name("<font color=lime>x</font>", "<c.lime>x</c>")]
#[case::hex_near_a_class(r##"<font color="#fe0101">x</font>"##, "<c.red>x</c>")]
#[case::hex_far_from_every_class(r##"<font color="#123456">x</font>"##, "<c.color123456>x</c>")]
#[case::short_hex(r##"<font color="#ff0">x</font>"##, "<c.yellow>x</c>")]
// libse writes a named colour as `#rrggbbaa`, and `ColorTranslator.FromHtml` reads
// that back as ARGB, so `Red` (ff0000ff) comes out as blue.
#[case::capitalised_name_through_its_argb_misreading(
    r#"<font color="Red">x</font>"#,
    "<c.blue>x</c>"
)]
#[case::unknown_capitalised_name(r#"<font color="Nope">x</font>"#, r#"<font color="Nope">x</c>"#)]
fn vtt_turns_font_colours_into_webvtt_classes(#[case] text: &str, #[case] expected: &str) {
    let s = subtitle_with(&[(text, 0.0, 1_000.0)]);
    assert_eq!(
        vtt::to_text(&s),
        format!("WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n{expected}")
    );
}

#[test]
fn vtt_keeps_a_webvtt_source_header_and_cue_region() {
    // Reachable once a WebVTT source is parsed (gl-473); the writer side is ported now.
    let mut s = subtitle_with(&[("x", 0.0, 1_000.0)]);
    s.header = "WEBVTT - a title\n\nREGION\nid:top\n".to_owned();
    s.paragraphs[0].region = Some("top".to_owned());
    assert_eq!(
        vtt::to_text(&s),
        "WEBVTT - a title\n\nREGION\nid:top\n\n00:00:00.000 --> 00:00:01.000 region:top\nx"
    );
}

// -- SSA / ASS writers -----------------------------------------------------

/// libse's default ASS header: its verbatim literal keeps the CRLF line ends of
/// libse's source file, the lines appended to it end in LF.
const ASS_DEFAULT_HEADER: &str = "[Script Info]\r\n; This is an Advanced Sub Station Alpha v4+ script.\r\nTitle: untitled\r\n\
ScriptType: v4.00+\nPlayDepth: 0\nScaledBorderAndShadow: Yes\r\n\r\n[V4+ Styles]\r\n\
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\r\n\
Style: Default,Arial,20,&H00FFFFFF,&H0000FFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,1,2,10,10,10,1\r\n\r\n[Events]\r\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";

#[test]
fn ass_to_text_without_a_source_header_uses_libses_default_header() {
    let s = subtitle_with(&[("line one\nline two", 1_180.0, 6_850.0)]);
    assert_eq!(
        ssa::to_text_ass(&s, "untitled"),
        format!(
            "{ASS_DEFAULT_HEADER}Dialogue: 0,0:00:01.18,0:00:06.85,Default,,0,0,0,,line one\\Nline two\n"
        )
    );
}

#[test]
fn ass_rounds_hundredths_half_away_from_zero_and_ssa_truncates() {
    let s = subtitle_with(&[("x", 1_995.0, 3_725_004.0)]);
    assert!(
        ssa::to_text_ass(&s, "untitled")
            .ends_with("Dialogue: 0,0:00:02.00,1:02:05.00,Default,,0,0,0,,x\n")
    );
    assert!(
        ssa::to_text_ssa(&s, "untitled")
            .ends_with("Dialogue: Marked=0,0:00:01.99,1:02:05.00,*Default,NTP,0000,0000,0000,,x\n")
    );
}

#[test]
fn ssa_to_text_without_a_source_header_uses_libses_default_header() {
    let s = subtitle_with(&[("x", 0.0, 1_000.0)]);
    assert_eq!(
        ssa::to_text_ssa(&s, "untitled"),
        "[Script Info]\r\n; This is a Sub Station Alpha v4 script.\r\nTitle: untitled\r\nScriptType: v4.00\r\nCollisions: Normal\r\n\
PlayDepth: 0\r\n\r\n[V4 Styles]\r\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\r\n\
Style: Default,Arial,20,16777215,65535,0,0,0,0,1,1,1,2,10,10,10,0,1\r\n\r\n[Events]\r\n\
Format: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: Marked=0,0:00:00.00,0:00:01.00,*Default,NTP,0000,0000,0000,,x\n"
    );
}

#[rstest]
#[case::italic_bold_underline(
    "<i>i</i> <b>b</b> <u>u</u>",
    "{\\i1}i{\\i0} {\\b1}b{\\b0} {\\u1}u{\\u0}"
)]
#[case::colour_name(r#"<font color="red">x</font> y"#, "{\\c&H0000ff&}x{\\c} y")]
#[case::colour_closing_at_the_end(r##"<font color="#00ff00">x</font>"##, "{\\c&H00ff00&}x")]
#[case::face_and_size_merged(
    r#"<font face="Arial" size="20">x</font> y"#,
    "{\\fs20\\fnArial}x{\\fn\\fs} y"
)]
#[case::unknown_colour_is_white(r#"<font color="nope">x</font> y"#, "{\\c&Hffffff&}x{\\c} y")]
#[case::no_tags("plain", "plain")]
fn format_text_turns_html_into_override_tags(#[case] html: &str, #[case] ass: &str) {
    assert_eq!(ssa_tags::format_text(html), ass);
}

#[rstest]
#[case::italic_and_unclosed_bold("{\\i1}it{\\i0} {\\b1}open", "<i>it</i> <b>open</b>")]
#[case::unsupported_tag_leaves_colours(
    "{\\pos(1,2)}{\\c&H0000FF&}red{\\c}",
    "{\\pos(1,2)}{\\c&H0000FF&}red{\\c}"
)]
#[case::primary_colour_size_and_face_with_italic(
    "{\\1c&HFF0000&}blue {\\fs22.5}size{\\fs} {\\fnA\\i1}face",
    r##"<font color="#0000ff">blue <font size="22.5">size</font> <font face="A"><i>face</font></font></i>"##
)]
#[case::line_breaks("a\\Nb\\nc", "a\nb\nc")]
// `rest.Length > 2` counts UTF-16 units: `cé` is two, so libse keeps it as an unknown
// tag, which the later `{\c` pass then reads as a colour.
#[case::accented_colour_sub_tag(
    "{\\fnArial\\cé}hello",
    r##"<font face="Arial"><font color="#000000">hello</font></font>"##
)]
fn get_formatted_text_turns_override_tags_into_html(#[case] ass: &str, #[case] html: &str) {
    assert_eq!(ssa_tags::get_formatted_text(ass), Ok(html.to_owned()));
}

// -- SSA / ASS loaders -----------------------------------------------------

#[test]
fn ass_load_keeps_the_header_footer_and_event_columns() {
    let input = lines(
        "[Script Info]\nTitle: t\n\n[V4+ Styles]\nStyle: Sign,Arial,20\n\n[Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 2,0:00:01.00,0:00:02.00,Sign, Alice ,0010,0020,0030, fx ,{\\i1}a, b\\Nc\n\n[Fonts]\nfontname: x.ttf\nAAAA",
    );
    let mut s = Subtitle::new();
    assert_eq!(ssa::load_ass(&mut s, &input), 0);
    assert_eq!(
        s.header,
        "[Script Info]\nTitle: t\n\n[V4+ Styles]\nStyle: Sign,Arial,20\n\n[Events]\n"
    );
    assert_eq!(s.footer, "[Fonts]\nfontname: x.ttf\nAAAA");
    let p = &s.paragraphs[0];
    assert_eq!(p.text, "{\\i1}a, b\nc");
    assert_eq!(p.extra.as_deref(), Some("Sign"));
    assert_eq!(p.actor.as_deref(), Some("Alice"));
    assert_eq!(
        (
            p.margin_l.as_deref(),
            p.margin_r.as_deref(),
            p.margin_v.as_deref()
        ),
        (Some("0010"), Some("0020"), Some("0030"))
    );
    assert_eq!(p.effect.as_deref(), Some("fx"));
    assert_eq!(p.layer, 2);
}

#[test]
fn ssa_load_turns_override_tags_into_html_and_keeps_column_spacing() {
    let input = lines(
        "[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: Marked=0,0:00:01.00,0:00:02.00, Song, Bob ,0,0,0,,{\\i1}it{\\i0}",
    );
    let mut s = Subtitle::new();
    assert_eq!(ssa::load_ssa(&mut s, &input), 0);
    let p = &s.paragraphs[0];
    assert_eq!(p.text, "<i>it</i>");
    assert_eq!(p.extra.as_deref(), Some(" Song"));
    assert_eq!(p.actor.as_deref(), Some(" Bob "));
}

#[rstest]
#[case::ass_starts_its_events_at_any_dialogue_line("ass", vec!["", "inside"])]
#[case::ssa_waits_for_the_events_section("ssa", vec!["inside"])]
fn a_dialogue_line_before_the_events_section(#[case] ext: &str, #[case] texts: Vec<&str>) {
    let input = lines(
        "[Script Info]\nDialogue: 0,0:00:01.00,0:00:02.00,x\n[Events]\nFormat: Layer, Start, End, Text\n\
         Dialogue: 0,0:00:01.00,0:00:02.00,inside",
    );
    let mut s = Subtitle::new();
    if ext == "ass" {
        ssa::load_ass(&mut s, &input);
    } else {
        ssa::load_ssa(&mut s, &input);
    }
    assert_eq!(
        s.paragraphs
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        texts
    );
}

#[test]
fn ssa_load_without_format_header_uses_defaults() {
    // No `Format:` line: the default columns, Text the tenth.
    let input = lines("[Events]\nDialogue: ,0:00:01.18,0:00:01.85,,,,,,,hello there");
    for load in [ssa::load_ass, ssa::load_ssa] {
        let mut s = Subtitle::new();
        assert_eq!(load(&mut s, &input), 0);
        assert_eq!(s.paragraphs[0].text, "hello there");
        assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 1_180.0);
    }
}

#[test]
fn a_custom_format_order_resets_the_ass_columns_but_not_the_ssa_ones() {
    let input = lines(
        "[Events]\nFormat: Start, End, Layer, Text\nDialogue: 0:00:02.00,0:00:03.50,0,hello, world",
    );
    let mut ass = Subtitle::new();
    assert_eq!(ssa::load_ass(&mut ass, &input), 0);
    assert_eq!(ass.paragraphs[0].text, "hello, world");
    assert_eq!(ass.paragraphs[0].end_time.total_milliseconds(), 3_500.0);
    // The SSA loader keeps `Name` at the fifth column, which here is the text's tail.
    let mut ssa_sub = Subtitle::new();
    assert_eq!(ssa::load_ssa(&mut ssa_sub, &input), 0);
    assert_eq!(ssa_sub.paragraphs[0].text, "hello");
    assert_eq!(ssa_sub.paragraphs[0].actor.as_deref(), Some(" world"));
}

#[test]
fn ssa_load_bad_timecode_counts_error() {
    let input =
        lines("[Events]\nFormat: Layer, Start, End, Text\nDialogue: 0,notatime,0:00:03.50,x");
    for load in [ssa::load_ass, ssa::load_ssa] {
        let mut s = Subtitle::new();
        assert_eq!(load(&mut s, &input), 1);
        assert!(s.paragraphs.is_empty());
    }
}

#[test]
fn comment_events_load_as_flagged_cues_numbered_as_if_absent() {
    // libse loads `Comment:` events as cues flagged `IsComment`. The encoder writes
    // them back only into SSA/ASS, so the loaders number the other cues as libse
    // numbers the script without its comments.
    let input = lines(
        "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,a comment\n\
         Dialogue: 0,0:00:02.00,0:00:03.00,Default,,0,0,0,,a line",
    );
    for load in [ssa::load_ass, ssa::load_ssa] {
        let mut s = Subtitle::new();
        assert_eq!(load(&mut s, &input), 0);
        let cues: Vec<_> = s
            .paragraphs
            .iter()
            .map(|p| (p.text.as_str(), p.is_comment, p.number))
            .collect();
        assert_eq!(cues, vec![("a comment", true, 0), ("a line", false, 1)]);
    }
}

#[test]
fn ssa_is_ssa_like_detects_dialogue_lines() {
    assert!(ssa::is_ssa_like(&[
        "Dialogue: ,0:00:01.18,0:00:01.85,x".to_owned()
    ]));
    assert!(ssa::is_ssa_like(&["  dialogue: whatever".to_owned()]));
    assert!(!ssa::is_ssa_like(&[
        "[Events]".to_owned(),
        "Format: a,b".to_owned()
    ]));
}

// -- JSON writer -----------------------------------------------------------

#[test]
fn json_writer_emits_track_events_envelope() {
    let s = subtitle_with(&[("Hello", 1_000.0, 2_000.0)]);
    let v: serde_json::Value = serde_json::from_str(&json_writer::to_text(&s)).expect("valid json");
    let events = v["TrackEvents"].as_array().expect("array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["Id"], "1");
    assert_eq!(events[0]["Text"], "Hello");
    assert_eq!(
        events[0]["StartPositionTicks"],
        1_000 * TICKS_PER_MILLISECOND
    );
    assert_eq!(events[0]["EndPositionTicks"], 2_000 * TICKS_PER_MILLISECOND);
}

#[test]
fn json_writer_empty_subtitle_has_empty_events() {
    let v: serde_json::Value =
        serde_json::from_str(&json_writer::to_text(&Subtitle::new())).unwrap();
    assert!(v["TrackEvents"].as_array().unwrap().is_empty());
}

// -- Extension-keyed parser ------------------------------------------------

#[test]
fn parser_supports_expected_extensions() {
    let p = SubtitleEditParser::new();
    for ext in ["srt", ".srt", "SRT", "subrip", "ssa", "ass"] {
        assert!(p.supports_file_extension(ext), "{ext} should be supported");
    }
    for ext in ["vtt", "webvtt", "sub", "unknown"] {
        assert!(
            !p.supports_file_extension(ext),
            "{ext} should be unsupported"
        );
    }
}

#[test]
fn parser_unsupported_extension_errors() {
    let err = SubtitleEditParser::new()
        .parse(b"whatever", "xyz")
        .unwrap_err();
    assert!(err.contains("Unsupported file extension"));
}

#[test]
fn parser_no_cues_errors_with_unsupported_format() {
    let err = SubtitleEditParser::new()
        .parse(b"not a subtitle at all", "srt")
        .unwrap_err();
    assert!(err.contains("Unsupported format"));
}

#[test]
fn parser_strips_bom_and_handles_crlf() {
    let data = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\nhi\r\n".as_bytes();
    let parsed = SubtitleEditParser::new().parse(data, "srt").unwrap();
    assert_eq!(parsed.paragraphs.len(), 1);
    assert_eq!(parsed.paragraphs[0].text, "hi");
}

#[test]
fn parser_routes_ssa_and_ass_to_their_own_loaders() {
    let data = b"[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                 Dialogue: Marked=0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}it{\\i0}\n";
    let p = SubtitleEditParser::new();
    assert_eq!(
        p.parse(data, "ssa").unwrap().paragraphs[0].text,
        "<i>it</i>"
    );
    assert_eq!(
        p.parse(data, "ass").unwrap().paragraphs[0].text,
        "{\\i1}it{\\i0}"
    );
}

#[test]
fn parser_to_track_info_projects_cues() {
    let parsed = SubtitleEditParser::new()
        .parse(b"1\n00:00:01,000 --> 00:00:02,000\nhi\n", "srt")
        .unwrap();
    let info = ferrofin_mediaencoding::subtitles::parser::to_track_info(&parsed);
    assert_eq!(info.track_events.len(), 1);
    assert_eq!(info.track_events[0].id, "1");
    assert_eq!(info.track_events[0].text, "hi");
    assert_eq!(
        info.track_events[0].start_position_ticks,
        1_000 * TICKS_PER_MILLISECOND
    );
}
