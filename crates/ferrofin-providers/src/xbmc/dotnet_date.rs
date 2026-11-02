//! .NET date-time patterns for the NFO `ReleaseDateFormat` setting.
//!
//! `DateTime.ToString(format, InvariantCulture)` writes the release dates and
//! `DateTime.TryParseExact(text, format, InvariantCulture, AssumeUniversal |
//! AdjustToUniversal)` reads them. Both accept standard single-letter formats
//! and custom patterns, and a malformed pattern throws `FormatException` when
//! writing and fails the parse when reading. This module reproduces both for
//! UTC values; behavior was fitted against a .NET 10 oracle (see the test data).

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};

/// Invariant-culture month names.
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Invariant-culture day names, Sunday first.
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// The custom pattern a standard single-letter format stands for
/// (`DateTimeFormatInfo.InvariantInfo`), for a UTC value.
fn standard_pattern(letter: char) -> Option<&'static str> {
    Some(match letter {
        'd' => "MM/dd/yyyy",
        'D' => "dddd, dd MMMM yyyy",
        'f' => "dddd, dd MMMM yyyy HH:mm",
        'F' | 'U' => "dddd, dd MMMM yyyy HH:mm:ss",
        'g' => "MM/dd/yyyy HH:mm",
        'G' => "MM/dd/yyyy HH:mm:ss",
        'm' | 'M' => "MMMM dd",
        'o' | 'O' => "yyyy'-'MM'-'dd'T'HH':'mm':'ss'.'fffffffK",
        'r' | 'R' => "ddd, dd MMM yyyy HH':'mm':'ss 'GMT'",
        's' => "yyyy'-'MM'-'dd'T'HH':'mm':'ss",
        't' => "HH:mm",
        'T' => "HH:mm:ss",
        'u' => "yyyy'-'MM'-'dd HH':'mm':'ss'Z'",
        'y' | 'Y' => "yyyy MMMM",
        _ => return None,
    })
}

/// The pattern `format` denotes: the default `G` for the empty string, a
/// standard pattern for one letter, otherwise `format` itself. `None` is the
/// `FormatException` case of a lone character that is not a standard format.
fn expand(format: &str) -> Option<String> {
    let mut chars = format.chars();
    match (chars.next(), chars.next()) {
        (None, _) => standard_pattern('G').map(str::to_owned),
        (Some(letter), None) => standard_pattern(letter).map(str::to_owned),
        _ => Some(format.to_owned()),
    }
}

/// One element of a custom pattern.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// Text copied verbatim (quoted, escaped, or an ordinary character).
    Literal(String),
    /// A repeated specifier letter and its repeat count.
    Spec(char, usize),
}

/// Splits a custom pattern the way `DateTimeFormat.ParseRepeatPattern` and the
/// quoting/escaping rules do. `None` is a malformed pattern.
fn tokenize(pattern: &str, parsing: bool) -> Option<Vec<Token>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let letter = chars[index];
        match letter {
            'd' | 'f' | 'F' | 'g' | 'h' | 'H' | 'K' | 'm' | 'M' | 's' | 't' | 'y' | 'z' => {
                let count = chars[index..].iter().take_while(|c| **c == letter).count();
                tokens.push(Token::Spec(letter, count));
                index += count;
            }
            ':' | '/' => {
                tokens.push(Token::Literal(letter.to_string()));
                index += 1;
            }
            '\'' | '"' => {
                let mut literal = String::new();
                index += 1;
                loop {
                    let ch = *chars.get(index)?;
                    index += 1;
                    if ch == letter {
                        break;
                    }
                    if ch == '\\' {
                        literal.push(*chars.get(index)?);
                        index += 1;
                    } else {
                        literal.push(ch);
                    }
                }
                tokens.push(Token::Literal(literal));
            }
            '%' => {
                // A lone specifier: the next character is one pattern element.
                let next = *chars.get(index + 1)?;
                if next == '%' {
                    return None;
                }
                if parsing {
                    // Parsing only validates the `%`; the specifier after it
                    // keeps its full repeat count (`%yyyy` is `yyyy`).
                    index += 1;
                    continue;
                }
                if "dfFghHKmMstyz".contains(next) {
                    tokens.push(Token::Spec(next, 1));
                } else {
                    tokens.push(Token::Literal(next.to_string()));
                }
                index += 2;
            }
            '\\' => {
                tokens.push(Token::Literal(chars.get(index + 1)?.to_string()));
                index += 2;
            }
            other => {
                tokens.push(Token::Literal(other.to_string()));
                index += 1;
            }
        }
    }
    Some(tokens)
}

/// Appends `value` zero-padded to at least `width` digits.
fn push_padded(out: &mut String, value: impl std::fmt::Display, width: usize) {
    use std::fmt::Write as _;
    let _ = write!(out, "{value:0width$}");
}

/// The `z`/`zz`/`zzz` text: .NET writes the signed offset of the **host's local
/// time zone** at that instant, even for a UTC `DateTime` (`K` is the
/// kind-aware one), as hours (`-6`), padded hours (`-06`) or `-06:00`.
fn local_offset(value: DateTime<Utc>, count: usize) -> String {
    let seconds = chrono::Local
        .offset_from_utc_datetime(&value.naive_utc())
        .local_minus_utc();
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    match count {
        1 => format!("{sign}{hours}"),
        2 => format!("{sign}{hours:02}"),
        _ => format!("{sign}{hours:02}:{minutes:02}"),
    }
}

/// `DateTime.ToString(format, CultureInfo.InvariantCulture)` for a UTC value.
///
/// `None` is the `FormatException` .NET throws for a malformed pattern.
#[must_use]
pub fn format_date_time(value: DateTime<Utc>, format: &str) -> Option<String> {
    let pattern = expand(format)?;
    let mut out = String::new();
    let hour12 = if value.hour().is_multiple_of(12) {
        12
    } else {
        value.hour() % 12
    };
    let ticks = u64::from(value.nanosecond() / 100);
    for token in tokenize(&pattern, false)? {
        match token {
            Token::Literal(text) => out.push_str(&text),
            Token::Spec(letter, count) => match letter {
                'd' if count <= 2 => push_padded(&mut out, value.day(), count),
                'd' => {
                    let name = DAYS[value.weekday().num_days_from_sunday() as usize];
                    out.push_str(if count == 3 { &name[..3] } else { name });
                }
                'f' | 'F' => {
                    if count > 7 {
                        return None;
                    }
                    let digits = format!("{ticks:07}");
                    let digits = &digits[..count];
                    if letter == 'f' {
                        out.push_str(digits);
                    } else {
                        let trimmed = digits.trim_end_matches('0');
                        if trimmed.is_empty() && out.ends_with('.') {
                            out.pop();
                        }
                        out.push_str(trimmed);
                    }
                }
                'g' => out.push_str("A.D."),
                'h' => push_padded(&mut out, hour12, count.min(2)),
                'H' => push_padded(&mut out, value.hour(), count.min(2)),
                'K' => out.push('Z'),
                'm' => push_padded(&mut out, value.minute(), count.min(2)),
                's' => push_padded(&mut out, value.second(), count.min(2)),
                'M' if count <= 2 => push_padded(&mut out, value.month(), count),
                'M' => {
                    let name = MONTHS[value.month0() as usize];
                    out.push_str(if count == 3 { &name[..3] } else { name });
                }
                't' => {
                    let designator = if value.hour() < 12 { "AM" } else { "PM" };
                    out.push_str(if count == 1 {
                        &designator[..1]
                    } else {
                        designator
                    });
                }
                'y' => {
                    let year = value.year();
                    if count <= 2 {
                        push_padded(&mut out, year % 100, count);
                    } else {
                        push_padded(&mut out, year, count);
                    }
                }
                'z' => out.push_str(&local_offset(value, count)),
                _ => return None,
            },
        }
    }
    Some(out)
}

/// Whether `format` is a pattern `DateTime.ToString` accepts, regardless of the
/// value formatted (an invalid one throws once any date is written).
#[must_use]
pub fn is_valid_format(format: &str) -> bool {
    format_date_time(Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(), format).is_some()
}

/// Why an exact parse produced no value.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The format string itself is malformed (.NET throws `FormatException`).
    BadFormat,
    /// The text does not match the format.
    NoMatch,
}

/// A cursor over the text being parsed.
struct Input<'a> {
    text: &'a [char],
    at: usize,
}

impl Input<'_> {
    /// Exactly `min..=max` ASCII digits, as a number.
    fn digits(&mut self, min: usize, max: usize) -> Option<u32> {
        let mut value = 0_u32;
        let mut count = 0;
        while count < max
            && let Some(digit) = self.text.get(self.at).and_then(|c| c.to_digit(10))
            && self.text[self.at].is_ascii_digit()
        {
            value = value.checked_mul(10)?.checked_add(digit)?;
            self.at += 1;
            count += 1;
        }
        (count >= min).then_some(value)
    }

    /// The text of `candidates` at the cursor, compared ignoring case.
    fn word(&mut self, candidates: &[&str]) -> Option<usize> {
        for (index, candidate) in candidates.iter().enumerate() {
            let length = candidate.chars().count();
            let Some(slice) = self.text.get(self.at..self.at + length) else {
                continue;
            };
            if slice
                .iter()
                .zip(candidate.chars())
                .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            {
                self.at += length;
                return Some(index);
            }
        }
        None
    }

    fn literal(&mut self, expected: &str) -> bool {
        for want in expected.chars() {
            if self.text.get(self.at) != Some(&want) {
                return false;
            }
            self.at += 1;
        }
        true
    }

    fn peek(&self) -> Option<char> {
        self.text.get(self.at).copied()
    }
}

/// The fields a pattern sets before they resolve to one instant.
#[derive(Default)]
struct Parsed {
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
    weekday: Option<u32>,
    hour: Option<u32>,
    minute: u32,
    second: u32,
    ticks: u32,
    two_digit_year: bool,
    twelve_hour: bool,
    pm: Option<bool>,
    offset_seconds: Option<i32>,
}

/// `DateTime.TryParseExact(text, format, InvariantCulture, AssumeUniversal |
/// AdjustToUniversal)`, resolving fields a pattern leaves out against `today`.
///
/// # Errors
///
/// [`ParseError::BadFormat`] for a pattern .NET rejects with `FormatException`;
/// [`ParseError::NoMatch`] when the text does not fit.
pub fn parse_exact_on(
    text: &str,
    format: &str,
    today: chrono::NaiveDate,
) -> Result<DateTime<Utc>, ParseError> {
    // Empty text and an empty format never match, whatever the pattern; only
    // then does a lone non-standard character throw.
    if text.is_empty() || format.is_empty() {
        return Err(ParseError::NoMatch);
    }
    let pattern = expand(format).ok_or(ParseError::BadFormat)?;
    let tokens = tokenize(&pattern, true).ok_or(ParseError::NoMatch)?;
    let chars: Vec<char> = text.chars().collect();
    let mut input = Input {
        text: &chars,
        at: 0,
    };
    let mut parsed = Parsed::default();
    for token in tokens {
        match token {
            Token::Literal(literal) => {
                if !input.literal(&literal) {
                    return Err(ParseError::NoMatch);
                }
            }
            Token::Spec(letter, count) => {
                parse_spec(&mut input, &mut parsed, letter, count).ok_or(ParseError::NoMatch)?;
            }
        }
    }
    if input.at != chars.len() {
        return Err(ParseError::NoMatch);
    }
    resolve(&parsed, today).ok_or(ParseError::NoMatch)
}

/// Parses one specifier at the cursor. `None` is a mismatch.
fn parse_spec(input: &mut Input, parsed: &mut Parsed, letter: char, count: usize) -> Option<()> {
    let width = |count: usize| if count == 1 { (1, 2) } else { (count, count) };
    match letter {
        'd' if count <= 2 => {
            let (min, max) = width(count);
            set(&mut parsed.day, input.digits(min, max)?)?;
        }
        'd' => {
            let names: Vec<&str> = if count == 3 {
                DAYS.iter().map(|d| &d[..3]).collect()
            } else {
                DAYS.to_vec()
            };
            set(
                &mut parsed.weekday,
                u32::try_from(input.word(&names)?).ok()?,
            )?;
        }
        'M' if count <= 2 => {
            let (min, max) = width(count);
            set(&mut parsed.month, input.digits(min, max)?)?;
        }
        'M' => {
            let names: Vec<&str> = if count == 3 {
                MONTHS.iter().map(|m| &m[..3]).collect()
            } else {
                MONTHS.to_vec()
            };
            set(
                &mut parsed.month,
                u32::try_from(input.word(&names)?).ok()? + 1,
            )?;
        }
        'y' => {
            let (min, max) = width(count.min(5));
            let year = i32::try_from(input.digits(min, max)?).ok()?;
            // Repeats compare the digits as written; the century comes later.
            parsed.two_digit_year |= count <= 2;
            set(&mut parsed.year, year)?;
        }
        'h' | 'H' | 'm' | 's' if count <= 2 => {
            let (min, max) = width(count);
            let value = input.digits(min, max)?;
            match letter {
                'h' => {
                    parsed.twelve_hour = true;
                    set(&mut parsed.hour, value)?;
                }
                'H' => set(&mut parsed.hour, value)?,
                'm' => parsed.minute = value,
                _ => parsed.second = value,
            }
        }
        'f' if count <= 7 => {
            let digits = input.digits(count, count)?;
            parsed.ticks = digits * 10_u32.pow(u32::try_from(7 - count).ok()?);
        }
        'F' if count <= 7 => {
            let start = input.at;
            let digits = input.digits(0, count)?;
            let used = u32::try_from(input.at - start).ok()?;
            parsed.ticks = digits * 10_u32.pow(7 - used);
        }
        't' if count <= 2 => {
            parsed.pm = Some(if count == 1 {
                // The single-letter form compares exactly.
                let pm = input.peek() == Some('P');
                if !pm && input.peek() != Some('A') {
                    return None;
                }
                input.at += 1;
                pm
            } else {
                input.word(&["AM", "PM"])? == 1
            });
        }
        'g' if count <= 2 => {
            input.word(&["A.D.", "AD"])?;
        }
        'K' if count == 1 => {
            if input.peek() == Some('Z') {
                input.at += 1;
                parsed.offset_seconds = Some(0);
            } else if matches!(input.peek(), Some('+' | '-')) {
                parsed.offset_seconds = Some(offset(input, 3)?);
            }
        }
        'z' => parsed.offset_seconds = Some(offset(input, count)?),
        _ => return None,
    }
    Some(())
}

/// A field may repeat only with the same value (`yyyy yyyy` with `2000 2000`).
fn set<T: PartialEq>(slot: &mut Option<T>, value: T) -> Option<()> {
    match slot {
        Some(existing) if *existing != value => None,
        _ => {
            *slot = Some(value);
            Some(())
        }
    }
}

/// `z` (hours), `zz` (two-digit hours) or `zzz`/`K` (hours, an optional `:`
/// and two-digit minutes), signed; at most 14:00 either way.
fn offset(input: &mut Input, count: usize) -> Option<i32> {
    let sign = match input.peek()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    input.at += 1;
    let (min, max) = if count == 2 { (2, 2) } else { (1, 2) };
    let hours = i32::try_from(input.digits(min, max)?).ok()?;
    let minutes = if count >= 3 {
        if input.peek() == Some(':') {
            input.at += 1;
        }
        i32::try_from(input.digits(2, 2)?).ok()?
    } else {
        0
    };
    (minutes <= 59 && hours * 60 + minutes <= 14 * 60)
        .then_some(sign * (hours * 3600 + minutes * 60))
}

/// Combines the parsed fields, defaulting what a pattern omits.
fn resolve(parsed: &Parsed, today: chrono::NaiveDate) -> Option<DateTime<Utc>> {
    let year = parsed.year.map(|year| {
        if parsed.two_digit_year && year < 100 {
            // Calendar.TwoDigitYearMax is 2049 for the invariant culture.
            year + if year <= 49 { 2000 } else { 1900 }
        } else {
            year
        }
    });
    let date = if year.is_none() && parsed.month.is_none() && parsed.day.is_none() {
        today
    } else {
        chrono::NaiveDate::from_ymd_opt(
            year.unwrap_or_else(|| today.year()),
            parsed.month.unwrap_or(1),
            parsed.day.unwrap_or(1),
        )?
    };
    if let Some(weekday) = parsed.weekday
        && date.weekday().num_days_from_sunday() != weekday
    {
        return None;
    }
    let mut hour = parsed.hour.unwrap_or(0);
    if parsed.twelve_hour {
        if hour > 12 {
            return None;
        }
        hour %= 12;
        if parsed.pm == Some(true) {
            hour += 12;
        }
    } else if let Some(pm) = parsed.pm {
        // A designator must agree with a 24-hour hour, and PM needs one.
        match parsed.hour {
            Some(hour) if (hour >= 12) == pm => {}
            None if !pm => {}
            _ => return None,
        }
    }
    let time = chrono::NaiveTime::from_hms_nano_opt(
        hour,
        parsed.minute,
        parsed.second,
        parsed.ticks.checked_mul(100)?,
    )?;
    let mut moment = date.and_time(time).and_utc();
    if let Some(seconds) = parsed.offset_seconds {
        moment = moment.checked_sub_signed(chrono::TimeDelta::seconds(i64::from(seconds)))?;
    }
    // `DateTime` spans years 1 through 9999, before and after the offset shift.
    (1..=9999).contains(&date.year()).then_some(())?;
    (1..=9999).contains(&moment.year()).then_some(moment)
}

/// `TryReadDateTimeExact`'s parse: today's date fills what a pattern omits.
#[must_use]
pub fn try_parse_exact(text: &str, format: &str) -> Option<DateTime<Utc>> {
    parse_exact_on(text, format, chrono::Local::now().date_naive()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[format, ticks, expected]` rows from .NET 10 (`null` = FormatException).
    const FORMAT_CASES: &str = include_str!("../../tests/data/dotnet_date_format_cases.json");

    fn from_ticks(ticks: i64) -> DateTime<Utc> {
        // .NET ticks count 100 ns from 0001-01-01; chrono's epoch offset is
        // 62,135,596,800 s.
        let seconds = ticks / 10_000_000 - 62_135_596_800;
        let nanos = u32::try_from((ticks % 10_000_000) * 100).unwrap();
        Utc.timestamp_opt(seconds, nanos).unwrap()
    }

    #[test]
    fn formatting_matches_dotnet() {
        let rows: Vec<(String, i64, Option<String>)> = serde_json::from_str(FORMAT_CASES).unwrap();
        let bad: Vec<_> = rows
            .iter()
            // `z` is the host's local offset; it has its own test.
            .filter(|(format, _, _)| !format.contains('z'))
            .filter(|(format, ticks, expected)| {
                format_date_time(from_ticks(*ticks), format) != *expected
            })
            .take(25)
            .map(|(format, ticks, expected)| {
                (
                    format,
                    ticks,
                    expected,
                    format_date_time(from_ticks(*ticks), format),
                )
            })
            .collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[test]
    fn z_is_the_local_offset_at_that_instant() {
        let value = from_ticks(630_000_000_000_000_000);
        let seconds = chrono::Local
            .offset_from_utc_datetime(&value.naive_utc())
            .local_minus_utc();
        let hours = seconds / 3600;
        assert_eq!(
            format_date_time(value, "zzz").unwrap(),
            format!(
                "{}{:02}:{:02}",
                if seconds < 0 { '-' } else { '+' },
                hours.abs(),
                (seconds.abs() / 60) % 60
            )
        );
        assert_eq!(
            format_date_time(value, "%z").unwrap().chars().next(),
            Some(if seconds < 0 { '-' } else { '+' })
        );
    }

    /// `[format, text, ticks | null | "FormatException"]` rows from .NET 10, run
    /// on 2026-10-10 (the date a pattern without date fields resolves to).
    const PARSE_CASES: &str = include_str!("../../tests/data/dotnet_date_parse_cases.json");

    #[test]
    fn exact_parsing_matches_dotnet() {
        let rows: Vec<(String, String, Option<String>)> =
            serde_json::from_str(PARSE_CASES).unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let bad: Vec<_> = rows
            .iter()
            .filter_map(|(format, text, expected)| {
                let actual = match parse_exact_on(text, format, today) {
                    Ok(value) => Some(
                        ((value.timestamp() + 62_135_596_800) * 10_000_000
                            + i64::from(value.nanosecond() / 100))
                        .to_string(),
                    ),
                    Err(ParseError::BadFormat) => Some("FormatException".to_owned()),
                    Err(ParseError::NoMatch) => None,
                };
                (actual != *expected).then_some((format, text, expected, actual))
            })
            .take(40)
            .collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }
}
