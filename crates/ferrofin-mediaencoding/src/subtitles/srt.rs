//! SubRip (`.srt`) parser and writer.
//!
//! Port of libse 4.0.12's `Nikse.SubtitleEdit.Core.SubtitleFormats.SubRip`
//! (`libse` has no Rust equivalent): `LoadSubtitle`, `ReadLine`,
//! `TryReadTimeCodesLine` and `IsValidTimeCode` line for line, which Jellyfin's
//! `SubtitleEditParser` runs for every `.srt` it converts, and `ToText`, which its
//! `SubtitleEncoder` writes with. Cue text lines are joined with `\n`: libse joins
//! with `Environment.NewLine`, and the reference server runs on Linux, where that
//! is a bare LF (the upstream `SrtParserTests` oracle is written against
//! `Environment.NewLine` for exactly that reason).
//!
//! libse's `.wsrt` italics (`<3x>`) are not reachable through Jellyfin:
//! `SubtitleEditParser` passes the bare extension (`srt`) as the file name, which
//! never ends in `.wsrt`.

use std::fmt::Write as _;

use super::dotnet::{int_try_parse, is_digit};
use super::model::{Paragraph, Subtitle, TimeCode};

/// The line separator libse uses when joining/emitting multi-line cue text —
/// `Environment.NewLine` on the Linux reference server.
pub(crate) const NEWLINE: &str = "\n";

/// libse's default frame rate, `GeneralSettings.CurrentFrameRate` (= its
/// `DefaultFrameRate`). Jellyfin's own code never changes it and ships no
/// `Settings.xml` for libse to read it from. About twenty libse loaders set it,
/// process wide, from the file they load (among them `TimedText10`,
/// `TimedTextImsc11`, `Dost`, `Ebu` and the `DCinema*` and `FinalCutPro*` formats),
/// but Ferrofin ports none of them (gl-768), so a SubRip file read as frames is
/// read at this rate. Upstream parity value.
const LIBSE_DEFAULT_FRAME_RATE: f64 = 23.976;

/// The largest fraction `TryReadTimeCodesLine` still takes for a frame count
/// (`_isMsFrames`): a file whose every fraction has two digits and is at most this
/// is read as frames. Upstream parity value.
const MAX_FRAMES_FRACTION: i32 = 30;

/// A timing line longer than this, in UTF-16 units, is cut after its second time
/// code (`TryReadTimeCodesLine`'s "subtitle position" repair, for DVD rips'
/// `X1:100 X2:100 Y1:100 Y2:100`). Upstream parity value.
const POSITION_INFO_MIN_LENGTH: usize = 30;

/// `TimeCode.BaseUnit`: milliseconds per second.
const BASE_UNIT: f64 = 1000.0;

/// `TryReadTimeCodesLine`'s `defaultSeparator`.
const DEFAULT_SEPARATOR: &str = " --> ";

/// libse appends a message to `Errors` only while they are shorter than this, in
/// UTF-16 units (`_errors.Length < 2000`), so the last one may cross it. Upstream
/// parity value.
const MAX_ERRORS_LENGTH: usize = 2000;

/// The `StringBuilder` libse's SubRip, SubStation Alpha and Advanced SubStation
/// Alpha loaders collect their `Errors` in, with its `Length` in UTF-16 units.
#[derive(Default)]
pub(crate) struct ErrorText {
    /// The messages, one line each.
    pub(crate) text: String,
    /// `text`'s length in UTF-16 units, kept so the cap check costs nothing.
    units: usize,
}

impl ErrorText {
    /// `AppendLine(message)` while the text is under [`MAX_ERRORS_LENGTH`];
    /// whether it was appended.
    pub(crate) fn append(&mut self, message: std::fmt::Arguments<'_>) -> bool {
        let room = self.units < MAX_ERRORS_LENGTH;
        if room {
            let start = self.text.len();
            let _ = self.text.write_fmt(message);
            self.text.push_str(NEWLINE);
            self.units += self.text[start..].encode_utf16().count();
        }
        room
    }
}

/// `SubRip.ExpectingLine`: the line `LoadSubtitle` reads next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectingLine {
    /// A cue number.
    Number,
    /// A timing line.
    TimeCodes,
    /// Cue text.
    Text,
}

/// The fields `SubRip.LoadSubtitle` keeps on the format instance while it reads.
///
/// libse's `_lastParagraph` is not kept: every `Paragraphs.Add` in the loop also
/// sets it, so it is always the paragraph last added, `subtitle.paragraphs.last()`
/// (Jellyfin creates a new format instance per parse, so it starts null).
struct Loader {
    /// `_errorCount`.
    error_count: i32,
    /// `_errors`.
    errors: ErrorText,
    /// `_lineNumber`: the 1-based number of the line being read.
    line_number: usize,
    /// `_isMsFrames`: every time code read so far had two-digit fractions of at
    /// most [`MAX_FRAMES_FRACTION`].
    is_ms_frames: bool,
    /// `_paragraph`: the cue being read.
    paragraph: Paragraph,
    /// `_expecting`.
    expecting: ExpectingLine,
}

/// Parses SubRip `lines` into `subtitle`, returning libse's `ErrorCount` and
/// `Errors`.
///
/// Port of `SubRip.LoadSubtitle`. Each line is trimmed at its end and of DEL
/// characters, then read as the state machine expects; a time code line where a
/// number or more text was expected starts a cue (and makes libse renumber every
/// cue), malformed timing lines are repaired (the separator, `.` for `:`, missing
/// hours, a `-` for a negative time, DVD position info after the time codes), and
/// NUL characters in text become spaces. Last, when every fraction in the file had
/// two digits and was at most 30, they are read as frames at libse's default
/// 23.976 fps, and each cue's text is trimmed at its end.
///
/// `Errors` names each problem by its line: a line where a cue number was expected
/// and a timing line that does not read (both counted), a timing line with a field
/// out of range and a cue number with no blank line before it (neither counted).
pub fn load(subtitle: &mut Subtitle, lines: &[String]) -> (i32, String) {
    let mut do_renumber = false;
    let mut loader = Loader {
        error_count: 0,
        errors: ErrorText::default(),
        line_number: 0,
        is_ms_frames: true,
        paragraph: Paragraph::default(),
        expecting: ExpectingLine::Number,
    };

    subtitle.paragraphs.clear();
    for (i, raw) in lines.iter().enumerate() {
        loader.line_number = i + 1;
        let line = raw.trim_end().trim_matches('\u{7f}'); // 127=delete ascii
        let next = lines.get(i + 1).map_or("", String::as_str);
        let next_next = lines.get(i + 2).map_or("", String::as_str);
        let next_next_next = lines.get(i + 3).map_or("", String::as_str);

        // A new line is missing between two paragraphs or no line number (buggy file).
        // Kept verbatim, though it never fires: an integer line is never a time code
        // line too.
        if loader.expecting == ExpectingLine::Text
            && i + 1 < lines.len()
            && !loader.paragraph.text.is_empty()
            && is_integer(line)
            && loader.try_read_time_codes_line(line.trim()).is_some()
        {
            if !loader.paragraph.text.is_empty() {
                loader.add_paragraph(subtitle);
            }
            loader.expecting = ExpectingLine::Number;
        }
        if loader.expecting == ExpectingLine::Number
            && loader.try_read_time_codes_line(line.trim()).is_some()
        {
            loader.expecting = ExpectingLine::TimeCodes;
            do_renumber = true;
        } else if !loader.paragraph.text.is_empty()
            && loader.expecting == ExpectingLine::Text
            && loader.try_read_time_codes_line(line.trim()).is_some()
        {
            loader.add_paragraph(subtitle);
            loader.expecting = ExpectingLine::TimeCodes;
            do_renumber = true;
        }

        loader.read_line(subtitle, line, next, next_next, next_next_next);
    }

    if !loader.paragraph.is_default() || loader.expecting == ExpectingLine::Text {
        subtitle
            .paragraphs
            .push(std::mem::take(&mut loader.paragraph));
    }

    if do_renumber {
        subtitle.renumber();
    }

    for p in &mut subtitle.paragraphs {
        if loader.is_ms_frames {
            p.start_time
                .set_milliseconds(frames_to_milliseconds_max_999(f64::from(
                    p.start_time.milliseconds(),
                )));
            p.end_time
                .set_milliseconds(frames_to_milliseconds_max_999(f64::from(
                    p.end_time.milliseconds(),
                )));
        }
        let trimmed = p.text.trim_end().len();
        p.text.truncate(trimmed);
    }

    (loader.error_count, loader.errors.text)
}

impl Loader {
    /// `subtitle.Paragraphs.Add(_paragraph); _lastParagraph = _paragraph;
    /// _paragraph = new Paragraph();`.
    fn add_paragraph(&mut self, subtitle: &mut Subtitle) {
        subtitle
            .paragraphs
            .push(std::mem::take(&mut self.paragraph));
    }

    /// `SubRip.ReadLine`: reads `line` as the line `expecting` names.
    fn read_line(
        &mut self,
        subtitle: &mut Subtitle,
        line: &str,
        next: &str,
        next_next: &str,
        next_next_next: &str,
    ) {
        match self.expecting {
            ExpectingLine::Number => {
                if let Some(number) = int_try_parse(line) {
                    self.paragraph.number = number;
                    self.expecting = ExpectingLine::TimeCodes;
                } else if !is_null_or_white_space(line) {
                    match subtitle.paragraphs.last_mut() {
                        Some(last) if last.number.wrapping_add(1).to_string() == next_next => {
                            let joined = format!("{}{NEWLINE}{}", last.text, line.trim());
                            joined.trim().clone_into(&mut last.text);
                        }
                        _ => {
                            // `FormatLanguage.LineNumberXExpectedNumberFromSourceLineY`
                            self.errors.append(format_args!(
                                "Line {} - expected subtitle number: {line}",
                                self.line_number
                            ));
                            self.error_count += 1;
                        }
                    }
                }
            }
            ExpectingLine::TimeCodes => {
                if let Some(read) = self.try_read_time_codes_line(line) {
                    if let Some(repaired) = &read.out_of_range {
                        // `validate`: reported, not counted, and the line still read.
                        self.errors.append(format_args!(
                            "Line {} - error reading time code: {repaired}",
                            self.line_number
                        ));
                    }
                    self.paragraph.start_time = read.start;
                    self.paragraph.end_time = read.end;
                    self.paragraph.text = String::new();
                    self.expecting = ExpectingLine::Text;
                } else if !is_null_or_white_space(line) {
                    // `FormatLanguage.LineNumberXErrorReadingTimeCodeFromSourceLineY`
                    self.errors.append(format_args!(
                        "Line {} - error reading time code: {line}",
                        self.line_number
                    ));
                    self.error_count += 1;
                    self.expecting = ExpectingLine::Number; // lets go to next paragraph
                }
            }
            ExpectingLine::Text => {
                self.read_text_line(subtitle, line, next, next_next, next_next_next);
            }
        }
    }

    /// The `ExpectingLine.Text` arm of `SubRip.ReadLine`.
    fn read_text_line(
        &mut self,
        subtitle: &mut Subtitle,
        line: &str,
        next: &str,
        next_next: &str,
        next_next_next: &str,
    ) {
        if is_integer(line)
            && (self.try_read_time_codes_line(next).is_some()
                || next.is_empty() && self.try_read_time_codes_line(next_next).is_some())
        {
            // line is integer and time code follows - could loose a number from text...
            self.add_paragraph(subtitle);
            self.expecting = ExpectingLine::Number;
            if let Some(n) = int_try_parse(line) {
                self.paragraph.number = n;
            }
            if line.trim() != get_last_number(&self.paragraph) {
                // `FormatLanguage.LineNumberXExpectedEmptyLine`, not counted.
                self.errors.append(format_args!(
                        "Line {}: Empty line expected, but found number ({line}) followed by time code.",
                        self.line_number
                    ),
                );
            }
        } else if let Some(read) = self.try_read_time_codes_line(line) {
            if self.paragraph.end_time.total_milliseconds() > 0.0 || !self.paragraph.text.is_empty()
            {
                subtitle
                    .paragraphs
                    .push(std::mem::take(&mut self.paragraph));
            }
            // libse reads the line a second time into the new paragraph, with the
            // same result.
            self.paragraph = Paragraph::new(String::new(), read.start, read.end);
            self.expecting = ExpectingLine::Text;
        } else if !is_null_or_white_space(line)
            || self.is_text(next)
            || self.is_text(next_next)
            || next_next_next == get_last_number(&self.paragraph)
        {
            if !self.paragraph.text.is_empty() {
                self.paragraph.text.push_str(NEWLINE);
            }
            self.paragraph
                .text
                .push_str(remove_bad_chars(line).trim_end());
        } else if line.is_empty() && self.paragraph.text.is_empty() {
            self.paragraph.text = String::new();
            if !next.is_empty()
                && (is_integer(next) || self.try_read_time_codes_line(next).is_some())
            {
                self.add_paragraph(subtitle);
                self.expecting = ExpectingLine::Number;
            }
        } else if line.is_empty() && next.is_empty() {
            // libse appends `RemoveBadChars(line).TrimEnd()` after the line break,
            // which for this empty line is nothing.
            self.paragraph.text.push_str(NEWLINE);
        } else {
            self.add_paragraph(subtitle);
            self.expecting = ExpectingLine::Number;
        }
    }

    /// `SubRip.IsText`: neither blank, a number, nor a time code line.
    fn is_text(&mut self, text: &str) -> bool {
        !(is_null_or_white_space(text)
            || is_integer(text)
            || self.try_read_time_codes_line(text.trim()).is_some())
    }

    /// `SubRip.TryReadTimeCodesLine`: the line read, when it is a time code line,
    /// and `_isMsFrames` cleared once one of its fractions cannot be a frame count.
    /// libse's `validate` only adds an error message, which its one caller, the
    /// timing line `ReadLine` expects, adds from [`TimeCodesLine::out_of_range`].
    fn try_read_time_codes_line(&mut self, input: &str) -> Option<TimeCodesLine> {
        let read = read_time_codes_line(input)?;
        if self.is_ms_frames && !read.fractions_can_be_frames {
            self.is_ms_frames = false;
        }
        Some(read)
    }
}

/// A time code line `TryReadTimeCodesLine` read.
struct TimeCodesLine {
    /// The start time, negative when its hours are written with a `-`.
    start: TimeCode,
    /// The end time, negative when its hours are written with a `-`.
    end: TimeCode,
    /// Both fractions have two digits and are at most [`MAX_FRAMES_FRACTION`].
    fractions_can_be_frames: bool,
    /// The repaired line, when an hours, minutes or seconds field is over 99 or a
    /// fraction over 999 (what libse's `validate` reports).
    out_of_range: Option<String>,
}

/// The parsing half of `SubRip.TryReadTimeCodesLine`.
///
/// Repairs the separator (` -> `, ` —> `, ` - > `, ` ->> `, …, to ` --> `), turns
/// Arabic and look-alike commas into `,` and zero-width spaces into spaces, drops
/// what follows the second time code in a line longer than
/// [`POSITION_INFO_MIN_LENGTH`], removes every space, reads `.` as `:` and a `:` or
/// `;` before a fraction as `,`, adds missing hours (`04:48,460 --> 04:52,364`), and
/// reads a `-` before the hours as a negative time. The index arithmetic is
/// libse's, on UTF-16 units.
fn read_time_codes_line(input: &str) -> Option<TimeCodesLine> {
    // Positions and lengths below are libse's, in the shape
    // `00:00:00,000 --> 00:00:00,000` (29 units) or, without hours,
    // `00:00,000 --> 00:00,000` (23 units).
    let s = input.trim_start_matches(['-', ' ']);
    if s.encode_utf16().count() < 10 || !s.chars().next().is_some_and(is_digit) {
        return None;
    }

    // Fix some badly formatted separator sequences - anything can happen if you manually edit ;)
    let line = input
        // An Arabic comma, U+F7C8 and `¡`.
        .replace(['\u{060C}', '\u{F7C8}', '\u{00A1}'], ",")
        // A zero-width space and a zero-width no-break space.
        .replace(['\u{200B}', '\u{FEFF}'], " ")
        .replace(" -> ", DEFAULT_SEPARATOR)
        .replace(" \u{2014}> ", DEFAULT_SEPARATOR) // em-dash
        .replace(" \u{2014}\u{2014}> ", DEFAULT_SEPARATOR) // em-dash
        .replace(" - > ", DEFAULT_SEPARATOR)
        .replace(" ->> ", DEFAULT_SEPARATOR)
        .replace(" -- > ", DEFAULT_SEPARATOR)
        .replace(" - -> ", DEFAULT_SEPARATOR)
        .replace(" -->> ", DEFAULT_SEPARATOR)
        .replace(" ---> ", DEFAULT_SEPARATOR)
        .replace("  ", " ")
        .replace(": ", ":")
        .replace(" :", ":");
    let mut units: Vec<u16> = line.trim().encode_utf16().collect();

    // Removed stuff after time codes - like subtitle position
    //  - example of position info: 00:02:26,407 --> 00:02:31,356  X1:100 X2:100 Y1:100 Y2:100
    if units.len() > POSITION_INFO_MIN_LENGTH {
        // 29 is the length of `00:00:00,000 --> 00:00:00,000`.
        if let Some(cut) = [29, 28, 27].into_iter().find(|&i| is_unit(&units, i, ' ')) {
            units.truncate(cut);
        }
    }

    // removes all extra spaces. The cut ends before a space, so it never splits a
    // surrogate pair and the round trip through UTF-16 is lossless.
    let line = String::from_utf16_lossy(&units).replace(' ', "");
    let line = line.replace("-->", DEFAULT_SEPARATOR);

    // Fix a few more cases of wrong time codes, seen this: 00.00.02,000 --> 00.00.04,000
    let line = line.trim().replace('.', ":");
    let mut units: Vec<u16> = line.encode_utf16().collect();
    // Positions 8 and 25 are the fractions' separators in `00:00:00,000 --> 00:00:00,000`.
    if units.len() >= 29 && (is_unit(&units, 8, ':') || is_unit(&units, 8, ';')) {
        units[8] = u16::from(b',');
    }
    if (29..=30).contains(&units.len()) && (is_unit(&units, 25, ':') || is_unit(&units, 25, ';')) {
        units[25] = u16::from(b',');
    }

    // allow missing hours as some (buggy) websites generate these time codes: 04:48,460 --> 04:52,364
    if units.len() == 23
        && is_unit(&units, 2, ':')
        && is_unit(&units, 5, ',')
        && is_unit(&units, 9, ' ')
        && is_unit(&units, 12, '>')
        && is_unit(&units, 13, ' ')
        && is_unit(&units, 16, ':')
        && is_unit(&units, 19, ',')
    {
        // `"00:" + line.Insert(14, "00:")`
        let hours = "00:".encode_utf16();
        units = hours
            .clone()
            .chain(units[..14].iter().copied())
            .chain(hours)
            .chain(units[14..].iter().copied())
            .collect();
    }

    if !is_valid_time_code(&units) {
        return None;
    }
    let repaired = String::from_utf16_lossy(&units);
    let line = repaired.replace("-->", ":").replace(' ', "");
    let parts: Vec<&str> = line.split([':', ',']).collect();
    if parts.len() < 8 {
        return None;
    }

    // `int.Parse`, which throws (and libse then returns false) where `TryParse` fails.
    let mut values = [0_i32; 8];
    for (value, part) in values.iter_mut().zip(&parts) {
        *value = int_try_parse(part)?;
    }
    let [
        start_hours,
        start_minutes,
        start_seconds,
        start_milliseconds,
        end_hours,
        end_minutes,
        end_seconds,
        end_milliseconds,
    ] = values;

    // A part `int.Parse` takes is ASCII, so its length in bytes is its UTF-16 length.
    let fractions_can_be_frames = parts[3].len() == 2
        && start_milliseconds <= MAX_FRAMES_FRACTION
        && parts[7].len() == 2
        && end_milliseconds <= MAX_FRAMES_FRACTION;

    let mut start = TimeCode::from_hms(
        start_hours,
        start_minutes,
        start_seconds,
        start_milliseconds,
    );
    if parts[0].starts_with('-') && start.total_milliseconds() > 0.0 {
        start = TimeCode::from_milliseconds(-start.total_milliseconds());
    }
    let mut end = TimeCode::from_hms(end_hours, end_minutes, end_seconds, end_milliseconds);
    if parts[4].starts_with('-') && end.total_milliseconds() > 0.0 {
        end = TimeCode::from_milliseconds(-end.total_milliseconds());
    }
    Some(TimeCodesLine {
        start,
        end,
        fractions_can_be_frames,
        out_of_range: field_out_of_range(&values).then_some(repaired),
    })
}

/// `validate`'s range check on the eight parts of a time code line: an hours,
/// minutes or seconds field over 99, or a fraction (parts 3 and 7) over 999.
fn field_out_of_range(values: &[i32; 8]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(i, &v)| v > if i % 4 == 3 { 999 } else { 99 })
}

/// Whether the UTF-16 unit at `index` is the ASCII character `c`.
fn is_unit(units: &[u16], index: usize, c: char) -> bool {
    units.get(index).copied() == u16::try_from(u32::from(c)).ok()
}

/// `SubRip.IsValidTimeCode`: whether `line` (UTF-16 units) has the shape
/// `h:m:s,f --> h:m:s,f`, white space anywhere, `,` or `:` before each fraction,
/// a `-` allowed to start the hours, minutes and start seconds, and nothing but a
/// `-` after the end fraction. libse replaced the regex
/// `^-?\d+:-?\d+:-?\d+[:,]-?\d+\s*-->\s*-?\d+:-?\d+:-?\d+[:,]-?\d+$` with this.
fn is_valid_time_code(line: &[u16]) -> bool {
    let mut step = 0;
    for &unit in line {
        // `None` for a surrogate, which is neither white space nor a digit in .NET.
        let ch = char::from_u32(u32::from(unit));
        if ch.is_some_and(char::is_whitespace) {
            continue;
        }
        let digit = ch.is_some_and(is_digit);
        match step {
            // start numbers
            0 | 2 | 4 | 9 | 11 if digit || ch == Some('-') => step += 1,
            // number, seconds, milliseconds
            1 | 3 | 10 | 12 | 5 | 13 | 6 | 14 if digit => {}
            // number
            1 | 3 | 10 | 12 if ch == Some(':') => step += 1,
            // seconds
            5 | 13 if ch == Some(',') || ch == Some(':') => step += 1,
            // milliseconds, then separator char 2
            6 | 14 | 7 if ch == Some('-') => step += 1,
            // separator char 3
            8 if ch == Some('>') => step += 1,
            _ => return false,
        }
    }
    true
}

/// `Utilities.IsInteger`: `int.TryParse`.
fn is_integer(s: &str) -> bool {
    int_try_parse(s).is_some()
}

/// `string.IsNullOrWhiteSpace`.
fn is_null_or_white_space(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// `SubRip.GetLastNumber`: the number after the paragraph's.
fn get_last_number(paragraph: &Paragraph) -> String {
    paragraph.number.wrapping_add(1).to_string()
}

/// `SubRip.RemoveBadChars`: NUL characters become spaces.
fn remove_bad_chars(line: &str) -> String {
    line.replace('\0', " ")
}

/// `SubtitleFormat.FramesToMillisecondsMax999(frames)` at libse's
/// [`LIBSE_DEFAULT_FRAME_RATE`]: rounded half away from zero, at most 999.
fn frames_to_milliseconds_max_999(frames: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)] // `(int)` of a value within ±41 709
    let ms =
        (frames * (BASE_UNIT / frame_for_calculation(LIBSE_DEFAULT_FRAME_RATE))).round() as i32;
    ms.min(999)
}

/// `SubtitleFormat.GetFrameForCalculation`: the exact NTSC rate for one of the
/// three rates it rounds (23.976, 29.97, 59.94), otherwise the rate itself. Kept
/// whole: only the 23.976 arm is reached, as only [`LIBSE_DEFAULT_FRAME_RATE`] is
/// passed.
fn frame_for_calculation(frame_rate: f64) -> f64 {
    const NTSC_TOLERANCE: f64 = 0.001;
    if (frame_rate - 23.976).abs() < NTSC_TOLERANCE {
        return 24000.0 / 1001.0;
    }
    if (frame_rate - 29.97).abs() < NTSC_TOLERANCE {
        return 30000.0 / 1001.0;
    }
    if (frame_rate - 59.94).abs() < NTSC_TOLERANCE {
        return 60000.0 / 1001.0;
    }
    frame_rate
}

/// Serializes `subtitle` to SubRip text.
///
/// Port of libse's `SubRip.ToText`, which Jellyfin 12 writes with: per cue its
/// number, the `hh:mm:ss,fff --> hh:mm:ss,fff` timing (`TimeCode.ToString`), the
/// text and a blank line; the whole trimmed, then two line ends. No BOM: Jellyfin
/// encodes the text with `Encoding.UTF8.GetBytes`.
#[must_use]
pub fn to_text(subtitle: &Subtitle) -> String {
    let mut out = String::new();
    for p in &subtitle.paragraphs {
        let _ = write!(
            out,
            "{}{NEWLINE}{} --> {}{NEWLINE}{}{NEWLINE}{NEWLINE}",
            p.number,
            p.start_time.to_srt_string(),
            p.end_time.to_srt_string(),
            p.text
        );
    }
    format!("{}{NEWLINE}{NEWLINE}", out.trim())
}
