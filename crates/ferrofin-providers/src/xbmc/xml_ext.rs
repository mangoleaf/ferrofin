//! Value-parsing helpers for the NFO cursor — port of
//! `MediaBrowser.Controller.Extensions.XmlReaderExtensions` plus the small
//! `TvParserHelpers` / `TVUtils.GetAirDays` helpers the parsers call, and the
//! internal `GetImageType` / `LeftPart` utilities. The IMDb/TMDb/TVDb id
//! scanners (`ProviderIdParsers`) are `ferrofin_common::providers`.
//!
//! These operate on the [`XmlCursor`] from [`super::xml_reader`] and re-create
//! the C# extension-method semantics one-for-one (invariant-culture parsing,
//! trimming, comma/pipe/semicolon splitting).

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use ferrofin_model::data::PersonKind;
use ferrofin_model::dto::DayOfWeek;
use ferrofin_model::entities::{ImageType, SeriesStatus};

use crate::container_types::PersonInfo;

use super::xml_reader::XmlCursor;

/// Reads a trimmed string from the current node
/// (`XmlReaderExtensions.ReadNormalizedString`).
pub fn read_normalized_string(reader: &mut XmlCursor) -> String {
    reader.read_element_content_as_string().trim().to_owned()
}

/// Reads an `i32` from the current node (`XmlReaderExtensions.TryReadInt`).
///
/// `int.TryParse(s, InvariantCulture)`: the grammar of [`parse_int`], with no
/// value instead of an exception.
pub fn try_read_int(reader: &mut XmlCursor) -> Option<i32> {
    parse_int(&reader.read_element_content_as_string()).ok()
}

/// The exception a typed read (`ReadElementContentAsBoolean`,
/// `ReadElementContentAsInt`) throws, which ends the parse where it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// An `XmlException`: a value that does not convert, or a child element in
    /// the element. The parsers that catch `XmlException` keep what they read
    /// before it, as they do for a syntax error.
    Xml,
    /// An exception no parser catches: the `FormatException` of an empty
    /// element, whose `XmlConvert.ToXxx(string.Empty)` runs outside the
    /// `XmlException` wrapping, or the `OverflowException` of an integer out of
    /// range. It fails the local provider.
    Uncaught,
}

/// .NET's `int.Parse` with `NumberStyles.Integer` and the invariant culture,
/// which both `XmlConvert.ToInt32` and `int.TryParse(s, InvariantCulture)` run:
/// an optional `+`/`-` and ASCII digits, white space (tab to CR, and space)
/// around them, then any NULs. A bad format is [`ReadError::Xml`] (the
/// `FormatException` `ReadContentAsInt` wraps) and wins over an overflow, which
/// is [`ReadError::Uncaught`].
fn parse_int(text: &str) -> Result<i32, ReadError> {
    let white = |c: char| c == ' ' || ('\t'..='\r').contains(&c);
    let number = text
        .trim_start_matches(white)
        .trim_end_matches('\0')
        .trim_end_matches(white);
    let digits = number.strip_prefix(['+', '-']).unwrap_or(number);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ReadError::Xml);
    }
    number.parse().map_err(|_| ReadError::Uncaught)
}

/// Reads the current element's text and converts it, as
/// `ReadElementContentAsXxx` does: a child element before or after the text is
/// an `XmlException`, and an empty element is [`ReadError::Uncaught`]
/// (`XmlConvert` refuses `""` for both types read this way).
fn read_element_content_as<T>(
    reader: &mut XmlCursor,
    convert: impl FnOnce(&str) -> Result<T, ReadError>,
) -> Result<T, ReadError> {
    if reader.is_empty_element() {
        return Err(ReadError::Uncaught);
    }
    reader.read();
    if reader.is_element() {
        return Err(ReadError::Xml);
    }
    let Some(text) = reader.text_value() else {
        return Err(ReadError::Uncaught);
    };
    let value = convert(text)?;
    reader.read();
    if reader.is_element() {
        return Err(ReadError::Xml);
    }
    reader.read();
    Ok(value)
}

/// Reads a boolean from the current node
/// (`XmlReader.ReadElementContentAsBoolean`).
///
/// `XmlConvert.ToBoolean` takes exactly `true`, `false`, `1` or `0` after
/// trimming XML white space (space, tab, CR, LF).
///
/// # Errors
/// [`ReadError`] where .NET throws.
pub fn read_element_content_as_bool(reader: &mut XmlCursor) -> Result<bool, ReadError> {
    read_element_content_as(reader, |text| {
        match text.trim_matches([' ', '\t', '\n', '\r']) {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(ReadError::Xml),
        }
    })
}

/// Reads an `i32` from the current node (`XmlReader.ReadElementContentAsInt`,
/// whose `XmlConvert.ToInt32` is `int.Parse`).
///
/// # Errors
/// [`ReadError`] where .NET throws.
pub fn read_element_content_as_int(reader: &mut XmlCursor) -> Result<i32, ReadError> {
    read_element_content_as(reader, parse_int)
}

/// Parses a `DateTime` from the current node (`XmlReaderExtensions.TryReadDateTime`).
///
/// Mirrors `DateTime.TryParse(..., AssumeUniversal | AdjustToUniversal)` for the
/// formats that appear in Kodi NFO files: `yyyy-MM-dd HH:mm:ss`,
/// `yyyy-MM-ddTHH:mm:ss` and bare `yyyy-MM-dd`.
pub fn try_read_date_time(reader: &mut XmlCursor) -> Option<DateTime<Utc>> {
    parse_date_time_flexible(reader.read_element_content_as_string().trim())
}

/// Parses a `DateTime` from the current node using an exact format
/// (`XmlReaderExtensions.TryReadDateTimeExact`).
///
/// `DateTime.TryParseExact(content, format, InvariantCulture, AssumeUniversal |
/// AdjustToUniversal)`: the element text is matched as written (no trimming)
/// against any .NET date pattern, see [`super::dotnet_date`]. A malformed
/// pattern yields no date; upstream's `FormatException` would abort the read.
pub fn try_read_date_time_exact(reader: &mut XmlCursor, format: &str) -> Option<DateTime<Utc>> {
    super::dotnet_date::try_parse_exact(&reader.read_element_content_as_string(), format)
}

/// Parses a date-time in any of the NFO-supported layouts as UTC.
fn parse_date_time_flexible(text: &str) -> Option<DateTime<Utc>> {
    if text.is_empty() {
        return None;
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(text, fmt) {
            return Some(Utc.from_utc_datetime(&dt));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|dt| Utc.from_utc_datetime(&dt));
    }
    None
}

/// Splits comma/pipe/semicolon-delimited content into trimmed parts
/// (`XmlReaderExtensions.GetStringArray`).
///
/// Splits on comma only when neither `|` nor `;` is present (so names like
/// "Matthew, Jr." survive when pipes are used as the delimiter), otherwise on
/// `|`/`;`. Leading/trailing separators are trimmed first. Empty parts drop.
pub fn get_string_array(reader: &mut XmlCursor) -> Vec<String> {
    let value = reader.read_element_content_as_string();
    let use_pipe = value.contains('|') || value.contains(';');
    let separators: &[char] = if use_pipe { &['|', ';'] } else { &[','] };

    value
        .trim()
        .trim_matches(|c| separators.contains(&c))
        .split(|c| separators.contains(&c))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Parses a `PersonInfo` array of the given kind from delimited content
/// (`XmlReaderExtensions.GetPersonArray`).
pub fn get_person_array(reader: &mut XmlCursor, kind: PersonKind) -> Vec<PersonInfo> {
    get_string_array(reader)
        .into_iter()
        .map(|name| PersonInfo {
            name,
            type_: kind,
            ..PersonInfo::default()
        })
        .collect()
}

/// Parses a `PersonInfo` from a `<actor>` (or similar) subtree node
/// (`XmlReaderExtensions.GetPersonFromXmlNode`).
///
/// Returns `None` for an empty element or a person with a blank name. Missing
/// `<type>` defaults to [`PersonKind::Actor`]; one that does not parse is
/// [`PersonKind::Unknown`]. Recognized child tags:
/// `name`/`Name`, `role`/`Role`, `type`/`Type`, `order`/`sortorder`/`SortOrder`,
/// `thumb`.
pub fn get_person_from_xml_node(reader: &mut XmlCursor) -> Option<PersonInfo> {
    if reader.is_empty_element() {
        reader.read();
        return None;
    }

    let mut name = String::new();
    let mut type_ = PersonKind::Actor;
    let mut role = String::new();
    let mut sort_order: Option<i32> = None;
    let mut image_url: Option<String> = None;

    let mut subtree = reader.read_subtree();
    subtree.move_to_content();
    subtree.read();

    while !subtree.eof() {
        if !subtree.is_element() {
            subtree.read();
            continue;
        }

        match subtree.name() {
            "name" | "Name" => name = read_normalized_string(&mut subtree),
            "role" | "Role" => role = read_normalized_string(&mut subtree),
            // `Enum.TryParse(text, true, out type)`: any member name in any
            // case; a value that does not parse leaves `default(PersonKind)`,
            // `Unknown` — only a missing `<type>` keeps `Actor`.
            "type" | "Type" => {
                type_ = ferrofin_model::json::enums::try_parse(
                    &subtree.read_element_content_as_string(),
                    true,
                )
                .unwrap_or(PersonKind::Unknown);
            }
            "order" | "sortorder" | "SortOrder" => {
                if let Some(v) = try_read_int(&mut subtree) {
                    sort_order = Some(v);
                }
            }
            "thumb" => image_url = Some(read_normalized_string(&mut subtree)),
            _ => subtree.skip(),
        }
    }

    if name.trim().is_empty() {
        return None;
    }

    Some(PersonInfo {
        name,
        // C# assigns Role unconditionally, so an empty role is `Some("")` here
        // (distinct from a never-set `None`).
        role: Some(role),
        type_,
        sort_order,
        image_url,
        ..PersonInfo::default()
    })
}

/// Returns the portion of `text` before the first `separator`
/// (`StringExtensions.LeftPart`); the whole string if the separator is absent.
#[must_use]
pub fn left_part(text: &str, separator: char) -> &str {
    text.split_once(separator).map_or(text, |(left, _)| left)
}

/// Maps a Kodi NFO `aspect` value to an [`ImageType`] (`BaseNfoParser.GetImageType`).
///
/// Unknown aspects (including `"poster"`) map to [`ImageType::Primary`].
#[must_use]
pub fn get_image_type(aspect: &str) -> ImageType {
    match aspect {
        "banner" => ImageType::Banner,
        "clearlogo" => ImageType::Logo,
        "discart" => ImageType::Disc,
        "landscape" => ImageType::Thumb,
        "clearart" => ImageType::Art,
        "fanart" => ImageType::Backdrop,
        _ => ImageType::Primary,
    }
}

/// Parses air days from a Kodi `airs_dayofweek` value (`TVUtils.GetAirDays`).
///
/// `"Daily"` expands to all seven days; a recognized weekday name yields that
/// single day; any other non-empty value yields an empty list; empty/absent
/// input yields `None`.
#[must_use]
pub fn get_air_days(day: Option<&str>) -> Option<Vec<DayOfWeek>> {
    let day = day?;
    if day.is_empty() {
        return None;
    }

    if day.eq_ignore_ascii_case("Daily") {
        return Some(vec![
            DayOfWeek::Sunday,
            DayOfWeek::Monday,
            DayOfWeek::Tuesday,
            DayOfWeek::Wednesday,
            DayOfWeek::Thursday,
            DayOfWeek::Friday,
            DayOfWeek::Saturday,
        ]);
    }

    match parse_day_of_week(day) {
        Some(value) => Some(vec![value]),
        None => Some(Vec::new()),
    }
}

/// Parses a [`DayOfWeek`] name case-insensitively (`Enum.TryParse<DayOfWeek>`).
fn parse_day_of_week(day: &str) -> Option<DayOfWeek> {
    const DAYS: [DayOfWeek; 7] = [
        DayOfWeek::Sunday,
        DayOfWeek::Monday,
        DayOfWeek::Tuesday,
        DayOfWeek::Wednesday,
        DayOfWeek::Thursday,
        DayOfWeek::Friday,
        DayOfWeek::Saturday,
    ];
    DAYS.into_iter()
        .find(|d| format!("{d:?}").eq_ignore_ascii_case(day))
}

/// Tries to parse a series status string (`TvParserHelpers.TryParseSeriesStatus`).
///
/// Matches the enum names case-insensitively, plus the extra aliases
/// `Pilot`/`Returning Series`/`Returning` → [`SeriesStatus::Continuing`] and
/// `Cancelled`/`Canceled` → [`SeriesStatus::Ended`].
#[must_use]
pub fn try_parse_series_status(status: &str) -> Option<SeriesStatus> {
    const STATUSES: [SeriesStatus; 3] = [
        SeriesStatus::Continuing,
        SeriesStatus::Ended,
        SeriesStatus::Unreleased,
    ];
    const CONTINUING: [&str; 3] = ["Pilot", "Returning Series", "Returning"];
    const ENDED: [&str; 2] = ["Cancelled", "Canceled"];

    if let Some(s) = STATUSES
        .into_iter()
        .find(|s| format!("{s:?}").eq_ignore_ascii_case(status))
    {
        return Some(s);
    }
    if CONTINUING.iter().any(|s| s.eq_ignore_ascii_case(status)) {
        return Some(SeriesStatus::Continuing);
    }
    if ENDED.iter().any(|s| s.eq_ignore_ascii_case(status)) {
        return Some(SeriesStatus::Ended);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{ReadError, parse_int};

    /// `int.Parse(s, NumberStyles.Integer, InvariantInfo)`: NULs may only end
    /// the text, after any trailing white space; the full `i32` range parses.
    #[rstest::rstest]
    #[case::trailing_nuls("5\0\0", Ok(5))]
    #[case::white_space_then_nul(" 5 \0", Ok(5))]
    #[case::nul_then_white_space("5\0 ", Err(ReadError::Xml))]
    #[case::nul_first("\0 5", Err(ReadError::Xml))]
    #[case::vertical_tab_and_form_feed("\u{b}5\u{c}", Ok(5))]
    #[case::sign_alone("-", Err(ReadError::Xml))]
    #[case::two_signs("+-5", Err(ReadError::Xml))]
    #[case::min("-2147483648", Ok(i32::MIN))]
    #[case::below_min("-2147483649", Err(ReadError::Uncaught))]
    #[case::above_max("2147483648", Err(ReadError::Uncaught))]
    #[case::leading_zeros("-0000000000002147483648", Ok(i32::MIN))]
    fn parse_int_follows_dotnet(#[case] text: &str, #[case] expected: Result<i32, ReadError>) {
        assert_eq!(parse_int(text), expected, "{text:?}");
    }
}
