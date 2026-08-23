//! WebVTT (`.vtt`) writer.
//!
//! Port of Jellyfin's `VttWriter` (`MediaBrowser.MediaEncoding/Subtitles`): a
//! UTF-8 BOM (the C# `StreamWriter` with `Encoding.UTF8` emits one), the
//! `WEBVTT` signature, one `Region:` block every cue is placed in, then each cue's
//! `start --> end region:subtitle line:90%` timing and text, blank-line separated.
//! A cue whose end is not after its start is stretched by one millisecond, and a
//! literal `\n` escape in the text becomes a space.

use super::model::Subtitle;

/// The line terminator the C# writer emits on Linux (`Environment.NewLine`).
const NEWLINE: &str = "\n";

/// The byte-order mark `StreamWriter(stream, Encoding.UTF8)` writes first.
const BOM: &str = "\u{feff}";

/// The region every cue is placed in (verbatim from `VttWriter`).
const REGION: &str =
    "Region: id:subtitle width:80% lines:3 regionanchor:50%,100% viewportanchor:50%,90%";

/// Serializes `subtitle` to WebVTT text.
///
/// Port of `VttWriter.Write`.
#[must_use]
pub fn to_text(subtitle: &Subtitle) -> String {
    let mut out = String::from(BOM);
    out.push_str("WEBVTT");
    out.push_str(NEWLINE);
    out.push_str(NEWLINE);
    out.push_str(REGION);
    out.push_str(NEWLINE);
    out.push_str(NEWLINE);
    for p in &subtitle.paragraphs {
        let start = p.start_time.total_milliseconds();
        // make sure the start and end times are different and sequential
        let end = if p.end_time.total_milliseconds() <= start {
            start + 1
        } else {
            p.end_time.total_milliseconds()
        };
        out.push_str(&format_timecode(start));
        out.push_str(" --> ");
        out.push_str(&format_timecode(end));
        out.push_str(" region:subtitle line:90%");
        out.push_str(NEWLINE);
        out.push_str(&escape_newlines(&p.text));
        out.push_str(NEWLINE);
        out.push_str(NEWLINE);
    }
    out
}

/// `NewlineEscapeRegex` (`\\n`, case-insensitive) → a space.
fn escape_newlines(text: &str) -> String {
    text.replace("\\n", " ").replace("\\N", " ")
}

/// Formats a millisecond position as WebVTT `HH:MM:SS.mmm`
/// (`{0:hh\:mm\:ss\.fff}` of a `TimeSpan`).
fn format_timecode(total_ms: i64) -> String {
    let total = total_ms.max(0);
    let millis = total % 1000;
    let seconds = (total / 1000) % 60;
    let minutes = (total / 60_000) % 60;
    let hours = (total / 3_600_000) % 24;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}
