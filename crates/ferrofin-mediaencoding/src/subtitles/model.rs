//! The in-memory subtitle model shared by the hand-ported parsers and writers.
//!
//! Port of the pieces of `Nikse.SubtitleEdit.Core.Common` that the Jellyfin
//! `SubtitleEditParser` / `SubtitleEncoder` actually touch: `TimeCode`,
//! `Paragraph`, and `Subtitle`. The libse library has no Rust equivalent, so
//! these types are hand-ported (rather than reused) to hold the parse result
//! before it is projected onto [`ferrofin_model::media_info::SubtitleTrackEvent`].

/// The number of 100-nanosecond ticks in one millisecond.
///
/// `TimeSpan.Ticks` in .NET counts 100 ns units; libse timecodes are stored in
/// milliseconds, so converting to ticks multiplies by this constant.
pub const TICKS_PER_MILLISECOND: i64 = 10_000;

/// A subtitle timecode, stored in whole milliseconds.
///
/// Port of `Nikse.SubtitleEdit.Core.Common.TimeCode`, reduced to the millisecond
/// backing store and the tick projection the encoder needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeCode {
    /// The total time, in whole milliseconds.
    total_milliseconds: i64,
}

impl TimeCode {
    /// Creates a timecode from a total number of milliseconds.
    #[must_use]
    pub fn from_milliseconds(total_milliseconds: i64) -> Self {
        Self { total_milliseconds }
    }

    /// Creates a timecode from a total number of 100 ns ticks.
    #[must_use]
    pub fn from_ticks(ticks: i64) -> Self {
        Self {
            total_milliseconds: ticks / TICKS_PER_MILLISECOND,
        }
    }

    /// The timecode as a total number of milliseconds.
    #[must_use]
    pub fn total_milliseconds(self) -> i64 {
        self.total_milliseconds
    }

    /// The timecode as a total number of 100 ns ticks (`TimeSpan.Ticks`).
    #[must_use]
    pub fn ticks(self) -> i64 {
        self.total_milliseconds * TICKS_PER_MILLISECOND
    }
}

/// A single subtitle cue: its ordinal number, text, and start/end timecodes.
///
/// Port of the fields of `Nikse.SubtitleEdit.Core.Common.Paragraph` the encoder
/// reads: `Number`, `Text`, `StartTime`, `EndTime`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paragraph {
    /// The 1-based cue number (`Paragraph.Number`).
    pub number: i32,
    /// The cue text (may contain embedded newlines).
    pub text: String,
    /// The cue start timecode.
    pub start_time: TimeCode,
    /// The cue end timecode.
    pub end_time: TimeCode,
}

impl Paragraph {
    /// Creates a paragraph with the given text and start/end timecodes.
    #[must_use]
    pub fn new(text: String, start_time: TimeCode, end_time: TimeCode) -> Self {
        Self {
            number: 0,
            text,
            start_time,
            end_time,
        }
    }
}

/// A parsed subtitle: an ordered list of [`Paragraph`] cues.
///
/// Port of `Nikse.SubtitleEdit.Core.Common.Subtitle`, reduced to the paragraph
/// list and the renumbering behaviour the parsers rely on.
#[derive(Debug, Clone, Default)]
pub struct Subtitle {
    /// The subtitle cues, in document order.
    pub paragraphs: Vec<Paragraph>,
}

impl Subtitle {
    /// Creates an empty subtitle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Assigns 1-based sequential numbers to every paragraph.
    ///
    /// Port of `Subtitle.Renumber()` (default start of 1); libse invokes this
    /// after a successful load so `Paragraph.Number` reflects document order.
    pub fn renumber(&mut self) {
        for (i, paragraph) in self.paragraphs.iter_mut().enumerate() {
            paragraph.number = i32::try_from(i).unwrap_or(i32::MAX) + 1;
        }
    }
}
