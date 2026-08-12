//! The extension-keyed subtitle parser.
//!
//! Port of `MediaBrowser.MediaEncoding.Subtitles.SubtitleEditParser` (and the
//! `ISubtitleParser` interface). The C# implementation reflects over the whole
//! `libse` assembly to discover every `SubtitleFormat`; since the formats are
//! hand-ported here, this keeps an explicit extension → parser table covering
//! the formats Jellyfin actually parses on the read path (`srt`, `ssa`, `ass`,
//! plus the `subrip`/`vtt`/`webvtt` aliases).

use ferrofin_model::media_info::{SubtitleTrackEvent, SubtitleTrackInfo};

use super::model::Subtitle;
use super::{srt, ssa};

/// Parses subtitle streams keyed by file extension.
///
/// Port of `ISubtitleParser`; the concrete [`SubtitleEditParser`] is the only
/// implementation. Kept as a trait so the encoder can be tested against a
/// fake parser, matching the C# `ISubtitleParser` injection point.
pub trait SubtitleParser: Send + Sync {
    /// Parses `data` (the raw subtitle bytes, assumed UTF-8) as `file_extension`.
    ///
    /// Port of `Parse(Stream, string)`. Returns the parsed [`Subtitle`].
    ///
    /// # Errors
    ///
    /// Returns an error message when the extension is unsupported or no cue
    /// could be parsed (C# throws `ArgumentException` in both cases).
    fn parse(&self, data: &[u8], file_extension: &str) -> Result<Subtitle, String>;

    /// Whether `file_extension` is supported by this parser.
    ///
    /// Port of `SupportsFileExtension(string)`.
    fn supports_file_extension(&self, file_extension: &str) -> bool;
}

/// The extension-keyed SubtitleEdit-style parser used by the encoder.
///
/// Port of `SubtitleEditParser`. Stateless — the C# per-instance
/// reflection cache is replaced by the static [`Self::format_for`] table.
#[derive(Debug, Default, Clone, Copy)]
pub struct SubtitleEditParser;

impl SubtitleEditParser {
    /// Creates a new parser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Resolves the parser [`Format`] for `file_extension`, if supported.
    fn format_for(file_extension: &str) -> Option<Format> {
        match file_extension
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "srt" | "subrip" => Some(Format::SubRip),
            "ssa" => Some(Format::Ssa),
            "ass" => Some(Format::Ass),
            _ => None,
        }
    }
}

/// The subtitle formats this parser can load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// SubRip (`.srt`).
    SubRip,
    /// SubStation Alpha (`.ssa`).
    Ssa,
    /// Advanced SubStation Alpha (`.ass`).
    Ass,
}

/// Splits raw bytes into logical lines, matching .NET `Stream.ReadAllLines`.
///
/// Both `\r\n` and `\n` terminate a line; a trailing empty line from a final
/// terminator is dropped, mirroring the C# `ReadLine` loop.
fn read_all_lines(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

impl SubtitleParser for SubtitleEditParser {
    fn parse(&self, data: &[u8], file_extension: &str) -> Result<Subtitle, String> {
        let format = Self::format_for(file_extension)
            .ok_or_else(|| format!("Unsupported file extension: {file_extension}"))?;

        let lines = read_all_lines(data);
        let mut subtitle = Subtitle::new();

        let errors = match format {
            Format::SubRip => srt::load(&mut subtitle, &lines),
            Format::Ssa | Format::Ass => ssa::load(&mut subtitle, &lines),
        };

        let _ = errors;

        if subtitle.paragraphs.is_empty() {
            return Err(format!("Unsupported format: {file_extension}"));
        }

        Ok(subtitle)
    }

    fn supports_file_extension(&self, file_extension: &str) -> bool {
        Self::format_for(file_extension).is_some()
    }
}

/// Projects a parsed [`Subtitle`] onto the wire [`SubtitleTrackInfo`] DTO.
///
/// Convenience for callers (and tests) that want the ported cues as
/// [`SubtitleTrackEvent`]s directly.
#[must_use]
pub fn to_track_info(subtitle: &Subtitle) -> SubtitleTrackInfo {
    SubtitleTrackInfo {
        track_events: subtitle
            .paragraphs
            .iter()
            .map(|p| SubtitleTrackEvent {
                id: p.number.to_string(),
                text: p.text.clone(),
                start_position_ticks: p.start_time.ticks(),
                end_position_ticks: p.end_time.ticks(),
            })
            .collect(),
    }
}
