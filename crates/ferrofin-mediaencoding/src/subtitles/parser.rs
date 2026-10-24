//! The extension-keyed subtitle parser.
//!
//! Port of `MediaBrowser.MediaEncoding.Subtitles.SubtitleEditParser` (and the
//! `ISubtitleParser` interface). The C# implementation reflects over the whole
//! `libse` assembly to discover every `SubtitleFormat` and keys them by
//! extension; since the formats are hand-ported here, this keeps an explicit
//! extension → formats table: `srt` (and the `subrip` codec alias), `ssa`,
//! `ass` and `vtt`.
//!
//! TODO(gl-768): libse registers more formats than these: a second `vtt` one
//! (`WebVTTFileWithLineNumber`), `smi` (`Sami`), the text `sub` formats and the
//! rest. Each unported one still takes the encoder's ffmpeg SRT conversion.

use ferrofin_model::media_info::{SubtitleTrackEvent, SubtitleTrackInfo};

use super::model::Subtitle;
use super::{srt, ssa, vtt};

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
/// reflection cache is replaced by the static [`Self::formats_for`] table.
#[derive(Debug, Default, Clone, Copy)]
pub struct SubtitleEditParser;

impl SubtitleEditParser {
    /// Creates a new parser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The formats registered for `file_extension`, in the order `Parse`
    /// tries them, if any.
    fn formats_for(file_extension: &str) -> Option<&'static [Format]> {
        match file_extension
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "srt" | "subrip" => Some(&[Format::SubRip]),
            "ssa" => Some(&[Format::Ssa]),
            "ass" => Some(&[Format::Ass]),
            "vtt" => Some(&[Format::WebVtt]),
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
    /// WebVTT (`.vtt`).
    WebVtt,
}

impl Format {
    /// libse's `SubtitleFormat.Name`, for the logs.
    fn name(self) -> &'static str {
        match self {
            Self::SubRip => "SubRip",
            Self::Ssa => "Sub Station Alpha",
            Self::Ass => "Advanced Sub Station Alpha",
            Self::WebVtt => "WebVTT",
        }
    }

    /// `LoadSubtitle` into `subtitle`, returning its `ErrorCount`.
    fn load(self, subtitle: &mut Subtitle, lines: &[String]) -> Result<i32, String> {
        match self {
            Self::SubRip => Ok(srt::load(subtitle, lines)),
            Self::Ssa => Ok(ssa::load_ssa(subtitle, lines)),
            Self::Ass => Ok(ssa::load_ass(subtitle, lines)),
            Self::WebVtt => vtt::load(subtitle, lines),
        }
    }
}

/// Splits raw bytes into logical lines, matching .NET `Stream.ReadAllLines`.
///
/// `\r\n`, `\n` and a bare `\r` each terminate a line (`StreamReader.ReadLine`);
/// a trailing empty line from a final terminator is dropped, mirroring the C#
/// `ReadLine` loop.
fn read_all_lines(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

impl SubtitleParser for SubtitleEditParser {
    /// Upstream tries each format registered for the extension until one
    /// loads with no errors, all into ONE `Subtitle`, so a format that failed
    /// leaves its cues for the next to add to (as read, a failed WebVTT load
    /// followed by `WebVTTFileWithLineNumber` duplicates every cue, or keeps
    /// only the last). Each format loads into its own here; the first clean
    /// load wins, and when none is clean the first format's cues stand, which
    /// is what upstream returns for an extension with one format.
    fn parse(&self, data: &[u8], file_extension: &str) -> Result<Subtitle, String> {
        let formats = Self::formats_for(file_extension)
            .ok_or_else(|| format!("Unsupported file extension: {file_extension}"))?;

        let lines = read_all_lines(data);
        let mut first: Option<Subtitle> = None;
        let mut clean: Option<Subtitle> = None;
        for format in formats {
            tracing::debug!(
                file_extension,
                parser = format.name(),
                "trying a subtitle format parser"
            );
            let mut subtitle = Subtitle::new();
            let errors = format.load(&mut subtitle, &lines)?;
            if errors == 0 {
                clean = Some(subtitle);
                break;
            }
            // Upstream logs this at error. Debug here: the SubRip loader does
            // not count errors as libse's does (gl-757), so a file libse
            // loads cleanly could report some on every request.
            tracing::debug!(
                errors,
                file_extension,
                parser = format.name(),
                "errors encountered while parsing a subtitle"
            );
            first.get_or_insert(subtitle);
        }
        let subtitle = clean.or(first).unwrap_or_default();

        if subtitle.paragraphs.is_empty() {
            return Err(format!("Unsupported format: {file_extension}"));
        }

        Ok(subtitle)
    }

    fn supports_file_extension(&self, file_extension: &str) -> bool {
        Self::formats_for(file_extension).is_some()
    }
}

/// Projects a parsed [`Subtitle`] onto the wire [`SubtitleTrackInfo`] DTO.
///
/// Convenience for callers (and tests) that want the ported cues as
/// [`SubtitleTrackEvent`]s directly. SSA/ASS `Comment:` events are left out, as the
/// encoder leaves them out of every non-SSA/ASS format.
#[must_use]
pub fn to_track_info(subtitle: &Subtitle) -> SubtitleTrackInfo {
    SubtitleTrackInfo {
        track_events: subtitle
            .paragraphs
            .iter()
            .filter(|p| !p.is_comment)
            .map(|p| SubtitleTrackEvent {
                id: p.number.to_string(),
                text: p.text.clone(),
                start_position_ticks: p.start_time.ticks(),
                end_position_ticks: p.end_time.ticks(),
            })
            .collect(),
    }
}
