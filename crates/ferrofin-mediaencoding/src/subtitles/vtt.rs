//! WebVTT (`.vtt`) writer.
//!
//! Hand-port of the write path of `Nikse.SubtitleEdit.Core.SubtitleFormats.WebVTT`,
//! reduced to what the Jellyfin `SubtitleEncoder` emits when converting to
//! `vtt`. Timecodes use the `HH:MM:SS.mmm` form and cues are separated by blank
//! lines under the leading `WEBVTT` signature.

use super::model::{Subtitle, TimeCode};
use super::srt::NEWLINE;

/// Serializes `subtitle` to WebVTT text.
///
/// Port of `WebVTT.ToText`: a `WEBVTT` header, then each cue's `start --> end`
/// timing (millisecond precision, `.` separator) and text, blank-line separated.
#[must_use]
pub fn to_text(subtitle: &Subtitle) -> String {
    let mut out = String::from("WEBVTT");
    out.push_str(NEWLINE);
    out.push_str(NEWLINE);
    for p in &subtitle.paragraphs {
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

/// Formats a timecode as WebVTT `HH:MM:SS.mmm`.
fn format_timecode(tc: TimeCode) -> String {
    let total = tc.total_milliseconds().max(0);
    let millis = total % 1000;
    let seconds = (total / 1000) % 60;
    let minutes = (total / 60_000) % 60;
    let hours = total / 3_600_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}
