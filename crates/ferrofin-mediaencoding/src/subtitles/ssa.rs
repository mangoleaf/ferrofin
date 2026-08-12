//! SubStation Alpha (`.ssa`) and Advanced SubStation Alpha (`.ass`) parsers and
//! writers.
//!
//! Hand-port of `Nikse.SubtitleEdit.Core.SubtitleFormats.SubStationAlpha` and
//! `AdvancedSubStationAlpha` (`libse` has no Rust equivalent), reduced to the
//! `[Events]`/`Dialogue:` load path the Jellyfin `SubtitleEditParser` drives and
//! the minimal writers the `SubtitleEncoder` needs.

use super::model::{Paragraph, Subtitle, TimeCode};
use super::srt::NEWLINE;

/// Parses SSA/ASS `lines` into `subtitle`, returning the number of errors.
///
/// Port of the `SubStationAlpha`/`AdvancedSubStationAlpha` load path: it reads
/// the `[Events]` section's `Format:` header to locate the `Start`, `End`, and
/// `Text` columns, then decodes each `Dialogue:` line. `\N` and `\n` escapes in
/// the text are converted to line breaks; the `Text` column (always last) may
/// itself contain the field separator, so it is taken as the remainder.
pub fn load(subtitle: &mut Subtitle, lines: &[String]) -> i32 {
    let mut errors = 0;
    let mut in_events = false;
    let mut start_idx = 1usize;
    let mut end_idx = 2usize;
    let mut text_idx = 9usize;
    let mut format_columns = 0usize;

    for line in lines {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();

        if lower.starts_with('[') && lower.ends_with(']') {
            in_events = lower == "[events]";
            continue;
        }

        if !in_events {
            continue;
        }

        if lower.starts_with("format:") {
            let columns: Vec<String> = trimmed["format:".len()..]
                .split(',')
                .map(|c| c.trim().to_ascii_lowercase())
                .collect();
            format_columns = columns.len();
            for (i, col) in columns.iter().enumerate() {
                match col.as_str() {
                    "start" => start_idx = i,
                    "end" => end_idx = i,
                    "text" => text_idx = i,
                    _ => {}
                }
            }
            continue;
        }

        if lower.starts_with("dialogue:") {
            match parse_dialogue(
                &trimmed["dialogue:".len()..],
                start_idx,
                end_idx,
                text_idx,
                format_columns,
            ) {
                Some(paragraph) => subtitle.paragraphs.push(paragraph),
                None => errors += 1,
            }
        }
    }

    subtitle.renumber();
    errors
}

/// Decodes a single `Dialogue:` body into a [`Paragraph`].
fn parse_dialogue(
    body: &str,
    start_idx: usize,
    end_idx: usize,
    text_idx: usize,
    format_columns: usize,
) -> Option<Paragraph> {
    // Split into at most `format_columns` fields so the trailing Text column
    // keeps any embedded commas. Fall back to `text_idx + 1` when no Format
    // header was seen.
    let max_splits = if format_columns > 0 {
        format_columns
    } else {
        text_idx + 1
    };
    let fields: Vec<&str> = body.splitn(max_splits, ',').collect();

    let start = parse_timecode(fields.get(start_idx)?.trim())?;
    let end = parse_timecode(fields.get(end_idx)?.trim())?;
    let raw = fields.get(text_idx)?;

    Some(Paragraph::new(decode_text(raw), start, end))
}

/// Converts SSA/ASS escapes (`\N`, `\n`) in cue text to line breaks.
fn decode_text(text: &str) -> String {
    text.replace("\\N", NEWLINE).replace("\\n", NEWLINE)
}

/// Parses an SSA/ASS `H:MM:SS.cc` timecode (centisecond precision).
fn parse_timecode(part: &str) -> Option<TimeCode> {
    let part = part.trim();
    let (hms, frac) = part.split_once('.')?;
    let mut it = hms.split(':');
    let hours: i64 = it.next()?.trim().parse().ok()?;
    let minutes: i64 = it.next()?.trim().parse().ok()?;
    let seconds: i64 = it.next()?.trim().parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    // The fractional part is centiseconds (two digits) in SSA/ASS.
    let centis: i64 = frac.trim().parse().ok()?;
    Some(TimeCode::from_milliseconds(
        (((hours * 60) + minutes) * 60 + seconds) * 1000 + centis * 10,
    ))
}

/// Whether `[Events]`/`Dialogue:` content is present in `lines`.
///
/// Both the SSA and ASS libse formats gate a successful load on finding at
/// least one `Dialogue:` line inside the `[Events]` section.
#[must_use]
pub fn is_ssa_like(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.trim().to_ascii_lowercase().starts_with("dialogue:"))
}

/// Formats an SSA/ASS `H:MM:SS.cc` timecode (centisecond precision).
fn format_timecode(tc: TimeCode) -> String {
    let total = tc.total_milliseconds().max(0);
    let centis = (total % 1000) / 10;
    let seconds = (total / 1000) % 60;
    let minutes = (total / 60_000) % 60;
    let hours = total / 3_600_000;
    format!("{hours}:{minutes:02}:{seconds:02}.{centis:02}")
}

/// Encodes cue text for an SSA/ASS `Dialogue:` line (line breaks → `\N`).
fn encode_text(text: &str) -> String {
    text.replace('\n', "\\N").replace('\r', "")
}

/// The SSA (v4) file header written before the `[Events]` section.
const SSA_HEADER: &str = "[Script Info]\r\nScriptType: v4.00\r\nCollisions: Normal\r\n\r\n[V4 Styles]\r\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\r\nStyle: Default,Arial,20,65535,65535,65535,-2147483640,-1,0,1,3,0,2,10,10,10,0,0\r\n\r\n[Events]\r\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n";

/// The ASS (v4+) file header written before the `[Events]` section.
const ASS_HEADER: &str = "[Script Info]\r\nScriptType: v4.00+\r\nCollisions: Normal\r\n\r\n[V4+ Styles]\r\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\r\nStyle: Default,Arial,20,&H00FFFFFF,&H0300FFFF,&H00000000,&H02000000,-1,0,0,0,100,100,0,0,1,2,2,2,10,10,10,1\r\n\r\n[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n";

/// Serializes `subtitle` to SSA (v4) text.
///
/// Port of `SubStationAlpha.ToText`, using the `Marked=0` dialogue prefix.
#[must_use]
pub fn to_text_ssa(subtitle: &Subtitle) -> String {
    let mut out = String::from(SSA_HEADER);
    for p in &subtitle.paragraphs {
        let line = format!(
            "Dialogue: Marked=0,{},{},Default,,0000,0000,0000,,{}",
            format_timecode(p.start_time),
            format_timecode(p.end_time),
            encode_text(&p.text)
        );
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out
}

/// Serializes `subtitle` to Advanced SubStation Alpha (v4+) text.
///
/// Port of `AdvancedSubStationAlpha.ToText`, using the `Layer` dialogue prefix.
#[must_use]
pub fn to_text_ass(subtitle: &Subtitle) -> String {
    let mut out = String::from(ASS_HEADER);
    for p in &subtitle.paragraphs {
        let line = format!(
            "Dialogue: 0,{},{},Default,,0000,0000,0000,,{}",
            format_timecode(p.start_time),
            format_timecode(p.end_time),
            encode_text(&p.text)
        );
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out
}
