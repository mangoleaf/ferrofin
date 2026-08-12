//! SubRip (`.srt`) parser and writer.
//!
//! Hand-port of `Nikse.SubtitleEdit.Core.SubtitleFormats.SubRip` (`libse` has no
//! Rust equivalent), reduced to the load/save behaviour the Jellyfin
//! `SubtitleEditParser` and `SubtitleEncoder` drive. Cue text lines are joined
//! with `\r\n` to match libse's `Environment.NewLine` on the reference platform,
//! so the ported oracle values are reproduced byte-for-byte.

use std::sync::LazyLock;

use regex::Regex;

use super::model::{Paragraph, Subtitle, TimeCode};

/// The line separator libse uses when joining/emitting multi-line cue text.
pub(crate) const NEWLINE: &str = "\r\n";

/// Matches a SubRip timing line: `HH:MM:SS,mmm --> HH:MM:SS,mmm`.
///
/// Byte-for-byte port of the libse `SubRip` timecode regex, tolerating `.` or
/// `,` as the millisecond separator and optional surrounding whitespace.
static TIMING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-?\d+:\d+:\d+[,.]\d+\s*-->\s*-?\d+:\d+:\d+[,.]\d+")
        .expect("SubRip timing regex is valid")
});

/// Parses SubRip `lines` into `subtitle`, returning the number of errors.
///
/// Port of `SubRip.LoadSubtitle`: a cue is a timing line optionally preceded by
/// a numeric id line, followed by one or more text lines terminated by a blank
/// line (or EOF). Blank lines *inside* a cue are preserved, matching the
/// `example2.srt` oracle.
pub fn load(subtitle: &mut Subtitle, lines: &[String]) -> i32 {
    let mut errors = 0;
    let mut i = 0;
    while i < lines.len() {
        // Skip blank separator lines between cues.
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }

        // An optional numeric id line precedes the timing line; libse keeps the
        // file's own cue number on the paragraph (e.g. `311`).
        let mut idx = i;
        let mut cue_number: Option<i32> = None;
        if !TIMING.is_match(&lines[idx])
            && let Ok(n) = lines[idx].trim().parse::<i32>()
        {
            cue_number = Some(n);
            idx += 1;
        }

        if idx >= lines.len() || !TIMING.is_match(&lines[idx]) {
            // Not a well-formed cue header; count an error and resynchronize.
            errors += 1;
            i += 1;
            continue;
        }

        let Some((start, end)) = parse_timing(&lines[idx]) else {
            errors += 1;
            i += 1;
            continue;
        };
        idx += 1;

        // Collect text lines until the next cue header or EOF.
        let mut text_lines: Vec<String> = Vec::new();
        while idx < lines.len() {
            // The next cue starts at an id-line-then-timing pair or a bare
            // timing line; peek to decide whether to stop.
            if is_cue_header(lines, idx) {
                break;
            }
            text_lines.push(lines[idx].clone());
            idx += 1;
        }

        // Trim a single trailing blank line that acted as the cue separator.
        while text_lines.last().is_some_and(|l| l.trim().is_empty()) {
            text_lines.pop();
        }

        let mut paragraph = Paragraph::new(text_lines.join(NEWLINE), start, end);
        // libse keeps the cue's own SubRip number; fall back to sequential when
        // the file omitted an id line.
        paragraph.number = cue_number
            .unwrap_or_else(|| i32::try_from(subtitle.paragraphs.len()).unwrap_or(i32::MAX) + 1);
        subtitle.paragraphs.push(paragraph);
        i = idx;
    }

    errors
}

/// Whether the cue starting at `idx` is a new cue header (id? + timing).
fn is_cue_header(lines: &[String], idx: usize) -> bool {
    if TIMING.is_match(&lines[idx]) {
        return true;
    }
    if lines[idx].trim().parse::<i64>().is_ok()
        && idx + 1 < lines.len()
        && TIMING.is_match(&lines[idx + 1])
    {
        return true;
    }
    false
}

/// Parses a `HH:MM:SS,mmm --> HH:MM:SS,mmm` timing line into two timecodes.
fn parse_timing(line: &str) -> Option<(TimeCode, TimeCode)> {
    let (left, right) = line.split_once("-->")?;
    Some((parse_timecode(left)?, parse_timecode(right)?))
}

/// Parses a single `HH:MM:SS,mmm` (or `.mmm`) timecode.
fn parse_timecode(part: &str) -> Option<TimeCode> {
    let part = part.trim();
    let part = part.replace('.', ",");
    let (hms, millis) = part.split_once(',')?;
    let mut it = hms.split(':');
    let hours: i64 = it.next()?.trim().parse().ok()?;
    let minutes: i64 = it.next()?.trim().parse().ok()?;
    let seconds: i64 = it.next()?.trim().parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    let millis: i64 = millis.trim().parse().ok()?;
    Some(TimeCode::from_milliseconds(
        (((hours * 60) + minutes) * 60 + seconds) * 1000 + millis,
    ))
}

/// Serializes `subtitle` to SubRip text.
///
/// Port of `SubRip.ToText`: each cue is `number`, the `start --> end` timing,
/// the text, and a trailing blank line, joined with `\r\n`.
#[must_use]
pub fn to_text(subtitle: &Subtitle) -> String {
    let mut out = String::new();
    for (i, p) in subtitle.paragraphs.iter().enumerate() {
        out.push_str(&(i + 1).to_string());
        out.push_str(NEWLINE);
        out.push_str(&format_timecode(p.start_time));
        out.push_str(" --> ");
        out.push_str(&format_timecode(p.end_time));
        out.push_str(NEWLINE);
        out.push_str(&p.text);
        out.push_str(NEWLINE);
        out.push_str(NEWLINE);
    }
    out
}

/// Formats a timecode as `HH:MM:SS,mmm`.
pub(crate) fn format_timecode(tc: TimeCode) -> String {
    let total = tc.total_milliseconds().max(0);
    let millis = total % 1000;
    let seconds = (total / 1000) % 60;
    let minutes = (total / 60_000) % 60;
    let hours = total / 3_600_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}
