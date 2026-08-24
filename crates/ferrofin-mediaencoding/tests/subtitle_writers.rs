//! Coverage for the hand-ported subtitle writers (`srt`/`ssa`/`vtt`/`json`),
//! the extension-keyed parser edge cases, and the `TimeCode`/`Paragraph` model.
//!
//! These are pure transformations over the in-memory `Subtitle` model and carry
//! no process/network I/O, so they exercise the writer/parser decision logic
//! directly.

use ferrofin_mediaencoding::subtitles::model::{
    Paragraph, Subtitle, TICKS_PER_MILLISECOND, TimeCode,
};
use ferrofin_mediaencoding::subtitles::{SubtitleEditParser, SubtitleParser};
use ferrofin_mediaencoding::subtitles::{json_writer, srt, ssa, vtt};

/// `StreamWriter.WriteLine` on the Linux reference server.
const NL: &str = "\n";

fn subtitle_with(cues: &[(&str, i64, i64)]) -> Subtitle {
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

// -- TimeCode / model ------------------------------------------------------

#[test]
fn timecode_tick_roundtrip() {
    let tc = TimeCode::from_milliseconds(1_234);
    assert_eq!(tc.total_milliseconds(), 1_234);
    assert_eq!(tc.ticks(), 1_234 * TICKS_PER_MILLISECOND);
    // from_ticks truncates to whole milliseconds.
    let back = TimeCode::from_ticks(12_345_678);
    assert_eq!(back.total_milliseconds(), 1_234);
}

#[test]
fn renumber_is_one_based_and_sequential() {
    let s = subtitle_with(&[("a", 0, 100), ("b", 200, 300), ("c", 400, 500)]);
    assert_eq!(
        s.paragraphs.iter().map(|p| p.number).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

// -- SRT writer ------------------------------------------------------------

#[test]
fn srt_to_text_formats_number_timing_and_text() {
    let s = subtitle_with(&[("Hello", 137_440, 140_375)]);
    let out = srt::to_text(&s);
    // `SrtWriter`: BOM first (StreamWriter with Encoding.UTF8), then the cue.
    let expected = format!("\u{feff}1{NL}00:02:17,440 --> 00:02:20,375{NL}Hello{NL}{NL}");
    assert_eq!(out, expected);
}

#[test]
fn srt_to_text_multiple_cues_and_hours() {
    let s = subtitle_with(&[("a", 0, 1_000), ("b", 3_661_500, 3_662_000)]);
    let out = srt::to_text(&s);
    assert!(out.contains("01:01:01,500 --> 01:01:02,000"));
    // Two cues -> two number lines.
    assert!(out.starts_with(&format!("\u{feff}1{NL}")));
    assert!(out.contains(&format!("{NL}2{NL}")));
}

#[test]
fn srt_writer_wraps_the_hour_component_and_spaces_literal_newline_escapes() {
    // `hh\:mm\:ss\,fff` is the 0–23 hour component; `NewLineEscapedRegex` (`\\n`,
    // case-insensitive) turns a literal backslash-n into a space.
    let s = subtitle_with(&[("one\\ntwo\\Nthree", 90_000_000, 90_001_000)]);
    let out = srt::to_text(&s);
    assert!(out.contains("01:00:00,000 --> 01:00:01,000"));
    assert!(out.contains("one two three"));
}

#[test]
fn srt_writer_clamps_negative_timecode_to_zero() {
    let s = subtitle_with(&[("x", -5_000, 1_000)]);
    let out = srt::to_text(&s);
    assert!(out.contains("00:00:00,000 --> 00:00:01,000"));
}

// -- VTT writer ------------------------------------------------------------

const VTT_HEADER: &str = "\u{feff}WEBVTT\n\nRegion: id:subtitle width:80% lines:3 \
                          regionanchor:50%,100% viewportanchor:50%,90%\n\n";

#[test]
fn vtt_to_text_matches_jellyfins_vtt_writer() {
    // BOM + signature + the region block, then `start --> end region:subtitle line:90%`,
    // the text, a blank line — byte for byte what VttWriter emits on Linux.
    let s = subtitle_with(&[("Hi", 61_500, 62_250)]);
    let out = vtt::to_text(&s);
    assert_eq!(
        out,
        format!("{VTT_HEADER}00:01:01.500 --> 00:01:02.250 region:subtitle line:90%\nHi\n\n")
    );
}

#[test]
fn vtt_empty_subtitle_is_header_only() {
    let out = vtt::to_text(&Subtitle::new());
    assert_eq!(out, VTT_HEADER);
}

#[test]
fn vtt_stretches_non_sequential_cues_and_unescapes_newlines() {
    // end <= start → end = start + 1 ms; a literal `\n` escape becomes a space.
    let s = subtitle_with(&[("a\\nb", 1_000, 1_000)]);
    let out = vtt::to_text(&s);
    assert!(
        out.contains("00:00:01.000 --> 00:00:01.001 region:subtitle line:90%\na b\n"),
        "{out}"
    );
}

// -- SSA / ASS writers -----------------------------------------------------

#[test]
fn ssa_to_text_is_jellyfins_ssa_writer() {
    // Jellyfin's `SsaWriter`: its own header, `hh:mm:ss.ff` times, `Dialogue: 0,…,Default,text`,
    // and every `\n` in the text replaced by the literal two characters `\n`.
    let s = subtitle_with(&[("line one\nline two", 1_180, 6_850)]);
    let out = ssa::to_text_ssa(&s);
    let expected = "\u{feff}[Script Info]\nTitle: Jellyfin transcoded SSA subtitle\nScriptType: v4.00\n\n[V4 Styles]\n\
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\n\
Style: Default,Arial,20,&H00FFFFFF,&H00FFFFFF,&H19333333,&H19333333,0,0,0,1,0,2,10,10,10,0,1\n\n[Events]\nFormat: Layer, Start, End, Style, Text\n\
Dialogue: 0,00:00:01.18,00:00:06.85,Default,line one\\nline two\n";
    assert_eq!(out, expected);
}

#[test]
fn ass_to_text_is_jellyfins_ass_writer() {
    let s = subtitle_with(&[("hi", 1_180, 6_850), ("two", 3_661_500, 3_662_000)]);
    let out = ssa::to_text_ass(&s);
    assert!(out.starts_with("\u{feff}[Script Info]\nTitle: Jellyfin transcoded ASS subtitle\nScriptType: v4.00+\n\n[V4+ Styles]\n"));
    assert!(out.contains("\nStyle: Default,Arial,20,&H00FFFFFF,&H00FFFFFF,&H19333333,&H910E0807,0,0,0,0,100,100,0,0,0,1,0,2,10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Text\n"));
    assert!(out.ends_with("Dialogue: 0,00:00:01.18,00:00:06.85,Default,hi\nDialogue: 0,01:01:01.50,01:01:02.00,Default,two\n"));
}

#[test]
fn ssa_writer_truncates_to_hundredths_and_wraps_the_hour_component() {
    // `TimeSpan.ToString(@"hh\:mm\:ss\.ff")`: `ff` truncates, `hh` is the 0–23 hour component.
    let s = subtitle_with(&[("x", 1_999, 90_000_000)]);
    let out = ssa::to_text_ssa(&s);
    assert!(out.ends_with("Dialogue: 0,00:00:01.99,01:00:00.00,Default,x\n"));
}

// -- SSA parser edge cases -------------------------------------------------

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

#[test]
fn ssa_load_without_format_header_uses_defaults() {
    // No `Format:` line -> default start/end/text indices (1,2,9). With only a
    // few fields before the text, the default text_idx=9 fallback path parses
    // the remainder.
    let lines: Vec<String> = "[Events]\nDialogue: ,0:00:01.18,0:00:01.85,,,,,,,hello there"
        .lines()
        .map(str::to_owned)
        .collect();
    let mut s = Subtitle::new();
    let errors = ssa::load(&mut s, &lines);
    assert_eq!(errors, 0);
    assert_eq!(s.paragraphs.len(), 1);
    assert_eq!(s.paragraphs[0].text, "hello there");
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 1_180);
}

#[test]
fn ssa_load_custom_format_column_order() {
    // Custom order with Text last (so embedded commas in text are preserved),
    // exercising the Format-header column-index resolution.
    let lines: Vec<String> =
        "[Events]\nFormat: Start, End, Layer, Text\nDialogue: 0:00:02.00,0:00:03.50,0,hello, world"
            .lines()
            .map(str::to_owned)
            .collect();
    let mut s = Subtitle::new();
    let errors = ssa::load(&mut s, &lines);
    assert_eq!(errors, 0);
    assert_eq!(s.paragraphs.len(), 1);
    assert_eq!(s.paragraphs[0].start_time.total_milliseconds(), 2_000);
    assert_eq!(s.paragraphs[0].end_time.total_milliseconds(), 3_500);
    // 4 format columns -> splitn(4) keeps the embedded comma in the Text field.
    assert_eq!(s.paragraphs[0].text, "hello, world");
}

#[test]
fn ssa_load_bad_timecode_counts_error() {
    let lines: Vec<String> =
        "[Events]\nFormat: Layer, Start, End, Text\nDialogue: 0,notatime,0:00:03.50,x"
            .lines()
            .map(str::to_owned)
            .collect();
    let mut s = Subtitle::new();
    let errors = ssa::load(&mut s, &lines);
    assert_eq!(errors, 1);
    assert!(s.paragraphs.is_empty());
}

#[test]
fn ssa_load_ignores_lines_outside_events_section() {
    let lines: Vec<String> = "[Script Info]\nDialogue: 0,0:00:01.00,0:00:02.00,x\n[Events]\nFormat: Layer, Start, End, Text\nDialogue: 0,0:00:01.00,0:00:02.00,inside"
        .lines()
        .map(str::to_owned)
        .collect();
    let mut s = Subtitle::new();
    ssa::load(&mut s, &lines);
    assert_eq!(s.paragraphs.len(), 1);
    assert_eq!(s.paragraphs[0].text, "inside");
}

// -- JSON writer -----------------------------------------------------------

#[test]
fn json_writer_emits_track_events_envelope() {
    let s = subtitle_with(&[("Hello", 1_000, 2_000)]);
    let json = json_writer::to_text(&s);
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
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
    let json = json_writer::to_text(&Subtitle::new());
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
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
    let p = SubtitleEditParser::new();
    let err = p.parse(b"whatever", "xyz").unwrap_err();
    assert!(err.contains("Unsupported file extension"));
}

#[test]
fn parser_no_cues_errors_with_unsupported_format() {
    let p = SubtitleEditParser::new();
    // Well-formed extension, but no parseable cue.
    let err = p.parse(b"not a subtitle at all", "srt").unwrap_err();
    assert!(err.contains("Unsupported format"));
}

#[test]
fn parser_strips_bom_and_handles_crlf() {
    let p = SubtitleEditParser::new();
    let data = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\nhi\r\n".as_bytes();
    let parsed = p.parse(data, "srt").unwrap();
    assert_eq!(parsed.paragraphs.len(), 1);
    assert_eq!(parsed.paragraphs[0].text, "hi");
}

#[test]
fn parser_to_track_info_projects_cues() {
    let p = SubtitleEditParser::new();
    let data = "1\n00:00:01,000 --> 00:00:02,000\nhi\n".as_bytes();
    let parsed = p.parse(data, "srt").unwrap();
    let info = ferrofin_mediaencoding::subtitles::parser::to_track_info(&parsed);
    assert_eq!(info.track_events.len(), 1);
    assert_eq!(info.track_events[0].id, "1");
    assert_eq!(info.track_events[0].text, "hi");
    assert_eq!(
        info.track_events[0].start_position_ticks,
        1_000 * TICKS_PER_MILLISECOND
    );
}
