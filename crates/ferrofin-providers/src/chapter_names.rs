//! Chapter-name normalization — port of
//! `FFProbeVideoInfo.NormalizeChapterNames` and the `TimeSpan.TryParse` test it
//! applies to each name.
//!
//! Ripping programs often leave a chapter's name empty or set it to its start
//! time (`00:12:34`). Jellyfin replaces both with the localized
//! `ChapterNameValue` template ("Chapter {0}"), numbered from `1`.
//!
//! Only whether a name is accepted by .NET's culture-sensitive
//! `TimeSpan.TryParse(string, out _)` matters here, not the value, so
//! [`is_time_span`] reproduces its accept/reject set (verified against a .NET 10
//! oracle over a corpus of 23k strings, kept in `tests/data/time_span_oracle.json`).
//! It implements the invariant/en-US grammar; a culture with other separators
//! is the separate ambient-culture divergence tracked by S33/S34.

use ferrofin_model::entities_media::ChapterInfo;

/// The largest day count whose ticks fit a signed 64-bit `TimeSpan`.
const MAX_DAYS: u128 = 10_675_199;
/// Ticks in one second (`TimeSpan.TicksPerSecond`).
const TICKS_PER_SECOND: u128 = 10_000_000;
/// Fraction digits `TimeSpan` keeps (100 ns resolution).
const FRACTION_DIGITS: usize = 7;

/// Replaces every blank name, and every name `TimeSpan.TryParse` accepts, with
/// `template` formatted with the chapter's 1-based position.
///
/// `template` is the localized `ChapterNameValue` string; its `{0}` placeholder
/// receives the position, as `string.Format(CultureInfo.InvariantCulture, …)`.
pub fn normalize_chapter_names(chapters: &mut [ChapterInfo], template: &str) {
    for (index, chapter) in chapters.iter_mut().enumerate() {
        let replace = chapter.name.as_deref().is_none_or(|name| {
            name.trim_matches(char::is_whitespace).is_empty() || is_time_span(name)
        });
        if replace {
            chapter.name = Some(template.replace("{0}", &(index + 1).to_string()));
        }
    }
}

/// Whether .NET `TimeSpan.TryParse(input, out _)` accepts `input`.
///
/// Accepted (surrounding whitespace and one leading `-` allowed, no space after
/// the sign):
///
/// - `d`, `h:m`, `h:m:s[.f]`, `d.h:m`, `d.h:m:s[.f]`, `d:h:m:s[.f]`;
/// - three colon-separated numbers whose first exceeds 23 read as `d:h:m`;
/// - an empty seconds field when a fraction follows (`1:02:.5`);
/// - hours ≤ 23, minutes and seconds ≤ 59, at most seven fraction digits, and a
///   total that fits a signed 64-bit tick count (`-` reaching one tick further).
#[must_use]
pub fn is_time_span(input: &str) -> bool {
    let trimmed = input.trim_matches(char::is_whitespace);
    let (negative, body) = trimmed
        .strip_prefix('-')
        .map_or((false, trimmed), |rest| (true, rest));
    if !body
        .chars()
        .all(|character| character.is_ascii_digit() || character == ':' || character == '.')
    {
        return false;
    }
    // Split into digit groups and the separators between them.
    let mut groups: Vec<&str> = Vec::new();
    let mut separators = String::new();
    let mut start = 0;
    for (position, character) in body.char_indices() {
        if character == ':' || character == '.' {
            groups.push(&body[start..position]);
            separators.push(character);
            start = position + 1;
        }
    }
    groups.push(&body[start..]);

    // Every group must hold digits, except an empty seconds field before a
    // fraction (`h:m:.f`, `d.h:m:.f`, `d:h:m:.f`).
    let fraction = seconds_may_be_empty(&separators);
    let empty_seconds = fraction.then(|| groups.len() - 2);
    if groups
        .iter()
        .enumerate()
        .any(|(index, group)| group.is_empty() && Some(index) != empty_seconds)
    {
        return false;
    }
    let (numbers, fraction_ticks) = if fraction {
        let digits = groups[groups.len() - 1];
        if digits.len() > FRACTION_DIGITS {
            return false;
        }
        (
            &groups[..groups.len() - 1],
            format!("{digits:0<7}").parse().unwrap_or(0),
        )
    } else {
        (&groups[..], 0)
    };
    let Some(values) = numbers
        .iter()
        .map(|group| field_value(group))
        .collect::<Option<Vec<u128>>>()
    else {
        return false;
    };
    let core = separators.trim_end_matches('.');
    let fields = match (core, values.as_slice(), fraction) {
        ("", [days], false) => (*days, 0, 0, 0),
        (":", [hours, minutes], false) => (0, *hours, *minutes, 0),
        // `h:m:s`, or `d:h:m` when the first number cannot be hours.
        ("::", [first, second, third], false) if *first > 23 => (*first, *second, *third, 0),
        ("::", [hours, minutes, seconds], _) => (0, *hours, *minutes, *seconds),
        (".:", [days, hours, minutes], false) => (*days, *hours, *minutes, 0),
        (".::" | ":::", [days, hours, minutes, seconds], _) => (*days, *hours, *minutes, *seconds),
        _ => return false,
    };
    let (days, hours, minutes, seconds) = fields;
    if days > MAX_DAYS + 1 || hours > 23 || minutes > 59 || seconds > 59 {
        return false;
    }
    let ticks =
        (((days * 24 + hours) * 60 + minutes) * 60 + seconds) * TICKS_PER_SECOND + fraction_ticks;
    let limit = if negative {
        1_u128 << 63
    } else {
        (1_u128 << 63) - 1
    };
    ticks <= limit
}

/// An all-digit field's value, or `None` when it is far beyond every field's range.
fn field_value(digits: &str) -> Option<u128> {
    let digits = digits.trim_start_matches('0');
    if digits.len() > 20 {
        None
    } else if digits.is_empty() {
        Some(0)
    } else {
        digits.parse().ok()
    }
}

/// Whether `separators` ends in a fraction after a colon, where the seconds
/// number may be missing (`h:m:.f`, `d.h:m:.f`, `d:h:m:.f`).
fn seconds_may_be_empty(separators: &str) -> bool {
    matches!(separators, "::." | ".::." | ":::.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[input, accepted]` rows produced by .NET 10 `TimeSpan.TryParse` with the
    /// invariant culture over the generated corpus.
    const ORACLE: &str = include_str!("../tests/data/time_span_oracle.json");

    #[test]
    fn time_span_acceptance_matches_dotnet() {
        let rows: Vec<(String, bool)> = serde_json::from_str(ORACLE).unwrap();
        assert!(rows.len() > 3000);
        let mismatches: Vec<_> = rows
            .iter()
            .filter(|(input, accepted)| is_time_span(input) != *accepted)
            .take(20)
            .collect();
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    fn named(names: &[Option<&str>]) -> Vec<ChapterInfo> {
        names
            .iter()
            .map(|name| ChapterInfo {
                name: name.map(str::to_owned),
                ..ChapterInfo::default()
            })
            .collect()
    }

    #[test]
    fn blank_and_time_names_become_numbered_chapters() {
        let mut chapters = named(&[
            None,
            Some(""),
            Some("  \t"),
            Some("00:12:34"),
            Some(" 1:02:03.5 "),
            Some("Opening"),
            Some("12"),
            Some("Chapter 01"),
        ]);
        normalize_chapter_names(&mut chapters, "Kapitel {0}");
        let names: Vec<_> = chapters
            .iter()
            .map(|c| c.name.as_deref().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "Kapitel 1",
                "Kapitel 2",
                "Kapitel 3",
                "Kapitel 4",
                "Kapitel 5",
                "Opening",
                // A bare number is a day count.
                "Kapitel 7",
                "Chapter 01",
            ]
        );
    }
}
