//! The in-memory subtitle model shared by the hand-ported parsers and writers.
//!
//! Port of the pieces of `Nikse.SubtitleEdit.Core.Common` (libse 4.0.12) that the
//! Jellyfin `SubtitleEditParser` / `SubtitleEncoder` and the libse writers touch:
//! `TimeCode`, `Paragraph`, and `Subtitle`. The libse library has no Rust
//! equivalent, so these types are hand-ported (rather than reused).

/// The number of 100-nanosecond ticks in one millisecond.
///
/// `TimeSpan.Ticks` in .NET counts 100 ns units; libse timecodes are stored in
/// milliseconds, so converting to ticks multiplies by this constant.
pub const TICKS_PER_MILLISECOND: i64 = 10_000;

/// A subtitle timecode, stored like libse's: a fractional number of milliseconds.
///
/// Port of `Nikse.SubtitleEdit.Core.Common.TimeCode`. libse keeps
/// `TotalMilliseconds` as a `double`, so a time shifted by a tick offset keeps its
/// sub-millisecond part, and every component the writers print is read back through
/// a .NET `TimeSpan` ([`Self::ticks`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TimeCode {
    /// The total time, in milliseconds (`TotalMilliseconds`).
    total_milliseconds: f64,
}

impl TimeCode {
    /// `TimeCode.MaxTimeTotalMilliseconds`: 99:59:59.999. A larger magnitude reads
    /// back as a zero `TimeSpan`.
    pub const MAX_TIME_TOTAL_MILLISECONDS: f64 = 359_999_999.0;

    /// Creates a timecode from a total number of milliseconds
    /// (`new TimeCode(double totalMilliseconds)`).
    #[must_use]
    pub fn from_milliseconds(total_milliseconds: f64) -> Self {
        Self { total_milliseconds }
    }

    /// Creates a timecode from 100 ns ticks (`new TimeCode(TimeSpan.FromTicks(ticks))`):
    /// `TimeSpan.TotalMilliseconds` is the tick count divided by 10 000, as a `double`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // .NET does the same `long` → `double` division
    pub fn from_ticks(ticks: i64) -> Self {
        Self {
            total_milliseconds: ticks as f64 / TICKS_PER_MILLISECOND as f64,
        }
    }

    /// `new TimeCode(int hours, int minutes, int seconds, int milliseconds)`.
    #[must_use]
    pub fn from_hms(hours: i32, minutes: i32, seconds: i32, milliseconds: i32) -> Self {
        // `hours * 60 * 60 * BaseUnit + …`: the int products, then double arithmetic.
        let total = f64::from(hours.wrapping_mul(3600)) * 1000.0
            + f64::from(minutes.wrapping_mul(60)) * 1000.0
            + f64::from(seconds) * 1000.0
            + f64::from(milliseconds);
        Self {
            total_milliseconds: total,
        }
    }

    /// The timecode as a total number of milliseconds (`TotalMilliseconds`).
    #[must_use]
    pub fn total_milliseconds(self) -> f64 {
        self.total_milliseconds
    }

    /// `TotalSeconds`: the milliseconds divided by 1000.
    #[must_use]
    pub fn total_seconds(self) -> f64 {
        self.total_milliseconds / 1000.0
    }

    /// The `TimeSpan` the timecode reads back as, in 100 ns ticks (`TimeSpan.Ticks`).
    ///
    /// `TimeSpan.FromMilliseconds(double)` truncates the scaled value toward zero, and
    /// libse returns an empty `TimeSpan` beyond [`Self::MAX_TIME_TOTAL_MILLISECONDS`].
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // `(long)ticks` in .NET truncates the same way
    pub fn ticks(self) -> i64 {
        let ms = self.total_milliseconds;
        if !(-Self::MAX_TIME_TOTAL_MILLISECONDS..=Self::MAX_TIME_TOTAL_MILLISECONDS).contains(&ms) {
            return 0;
        }
        #[allow(clippy::cast_precision_loss)]
        let ticks = ms * TICKS_PER_MILLISECOND as f64;
        ticks as i64
    }

    /// `TimeCode.Hours`: the `TimeSpan`'s hours plus its days times 24, so it never wraps.
    #[must_use]
    pub fn hours(self) -> i32 {
        // At most 99 hours (the MaxTime guard), so the narrowing cannot lose anything.
        i32::try_from(self.ticks() / (TICKS_PER_MILLISECOND * 3_600_000)).unwrap_or(0)
    }

    /// `TimeCode.Minutes`: the `TimeSpan`'s minutes component (negative for a negative time).
    #[must_use]
    pub fn minutes(self) -> i32 {
        i32::try_from(self.ticks() / (TICKS_PER_MILLISECOND * 60_000) % 60).unwrap_or(0)
    }

    /// `TimeCode.Seconds`: the `TimeSpan`'s seconds component.
    #[must_use]
    pub fn seconds(self) -> i32 {
        i32::try_from(self.ticks() / (TICKS_PER_MILLISECOND * 1000) % 60).unwrap_or(0)
    }

    /// `TimeCode.Milliseconds`: the `TimeSpan`'s milliseconds component.
    #[must_use]
    pub fn milliseconds(self) -> i32 {
        i32::try_from(self.ticks() / TICKS_PER_MILLISECOND % 1000).unwrap_or(0)
    }

    /// `TimeCode.ToString()`: `hh:mm:ss,fff`, hours not wrapped, and one leading `-`
    /// for a negative time (`PrefixSign`).
    #[must_use]
    pub fn to_srt_string(self) -> String {
        let s = format!(
            "{}:{}:{},{}",
            dotnet_int(self.hours(), 2),
            dotnet_int(self.minutes(), 2),
            dotnet_int(self.seconds(), 2),
            dotnet_int(self.milliseconds(), 3)
        );
        if self.total_milliseconds >= 0.0 {
            s
        } else {
            format!("-{}", s.replace('-', ""))
        }
    }
}

/// An `int` formatted with the .NET custom format of `digits` zeros (`{0:00}`,
/// `{0:000}`): zero-padded, a negative value keeping its sign before the padding.
#[must_use]
pub fn dotnet_int(value: i32, digits: usize) -> String {
    if value < 0 {
        format!("-{:0digits$}", value.unsigned_abs())
    } else {
        format!("{value:0digits$}")
    }
}

/// A single subtitle cue.
///
/// Port of the fields of `Nikse.SubtitleEdit.Core.Common.Paragraph` that the
/// loaders fill and the writers read. libse keeps the optional ones as nullable
/// strings, which an `Option` stands for here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Paragraph {
    /// The cue number (`Paragraph.Number`).
    pub number: i32,
    /// The cue text (may contain embedded newlines).
    pub text: String,
    /// The cue start timecode.
    pub start_time: TimeCode,
    /// The cue end timecode.
    pub end_time: TimeCode,
    /// `Extra`: the SSA/ASS style name of the cue.
    pub extra: Option<String>,
    /// `IsComment`: an SSA/ASS `Comment:` event.
    pub is_comment: bool,
    /// `Actor`: the SSA/ASS `Name` (or `Actor`) column.
    pub actor: Option<String>,
    /// `Region`: the WebVTT region the cue names, if any.
    pub region: Option<String>,
    /// `MarginL`: the SSA/ASS left margin, as written in the source.
    pub margin_l: Option<String>,
    /// `MarginR`: the SSA/ASS right margin, as written in the source.
    pub margin_r: Option<String>,
    /// `MarginV`: the SSA/ASS vertical margin, as written in the source.
    pub margin_v: Option<String>,
    /// `Effect`: the SSA/ASS `Effect` column.
    pub effect: Option<String>,
    /// `Layer`: the ASS layer (SSA's `Marked`).
    pub layer: i32,
    /// `Language`: the TTML language of the cue.
    pub language: Option<String>,
    /// `Style`: the TTML style of the cue, or for a WebVTT cue its raw cue settings
    /// (`line:10% align:start`, empty when it has none).
    pub style: Option<String>,
    /// `NewSection`: the cue starts a new TTML `div`.
    pub new_section: bool,
}

impl Paragraph {
    /// Creates a paragraph with the given text and start/end timecodes.
    #[must_use]
    pub fn new(text: String, start_time: TimeCode, end_time: TimeCode) -> Self {
        Self {
            text,
            start_time,
            end_time,
            ..Self::default()
        }
    }
}

/// A parsed subtitle: its cues, plus the source header and footer the writers reuse.
///
/// Port of `Nikse.SubtitleEdit.Core.Common.Subtitle`, reduced to the paragraph list,
/// `Header`, `Footer`, and the renumbering behaviour the parsers rely on.
#[derive(Debug, Clone, Default)]
pub struct Subtitle {
    /// The subtitle cues, in document order.
    pub paragraphs: Vec<Paragraph>,
    /// `Header`, or empty: the SSA/ASS script header (everything through `[Events]`),
    /// or for WebVTT the signature line and the `STYLE`, `REGION` and `NOTE` blocks
    /// before the first cue.
    pub header: String,
    /// `Footer`: the SSA/ASS `[Fonts]`/`[Graphics]` sections after the events, or empty.
    pub footer: String,
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
