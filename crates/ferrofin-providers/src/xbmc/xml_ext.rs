//! Value-parsing helpers for the NFO cursor — port of
//! `MediaBrowser.Controller.Extensions.XmlReaderExtensions` plus the small
//! `ProviderIdParsers` / `TvParserHelpers` / `TVUtils.GetAirDays` helpers the
//! parsers call, and the internal `GetImageType` / `LeftPart` utilities.
//!
//! These operate on the [`XmlCursor`] from [`super::xml_reader`] and re-create
//! the C# extension-method semantics one-for-one (invariant-culture parsing,
//! trimming, comma/pipe/semicolon splitting, the IMDb/TMDb/TVDb id scanners).

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
/// Uses invariant-culture integer parsing on the trimmed content.
pub fn try_read_int(reader: &mut XmlCursor) -> Option<i32> {
    reader.read_element_content_as_string().trim().parse().ok()
}

/// Reads a boolean from the current node
/// (`XmlReader.ReadElementContentAsBoolean`).
///
/// .NET accepts `true`/`false`/`1`/`0` (case-insensitively for the words).
pub fn read_element_content_as_bool(reader: &mut XmlCursor) -> Option<bool> {
    match reader.read_element_content_as_string().trim() {
        s if s.eq_ignore_ascii_case("true") || s == "1" => Some(true),
        s if s.eq_ignore_ascii_case("false") || s == "0" => Some(false),
        _ => None,
    }
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
/// Only the `yyyy-MM-dd` format string is used by the NFO configuration; it is
/// matched against a bare date (midnight UTC).
pub fn try_read_date_time_exact(reader: &mut XmlCursor, format: &str) -> Option<DateTime<Utc>> {
    let text = reader.read_element_content_as_string();
    let text = text.trim();
    if format == "yyyy-MM-dd" {
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| Utc.from_utc_datetime(&dt))
    } else {
        parse_date_time_flexible(text)
    }
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
/// `<type>` defaults to [`PersonKind::Actor`]. Recognized child tags:
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
            "type" | "Type" => {
                if let Some(parsed) = parse_person_kind(&subtree.read_element_content_as_string()) {
                    type_ = parsed;
                }
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

/// Parses a [`PersonKind`] name case-insensitively (`Enum.TryParse<PersonKind>`).
///
/// Covers every `PersonKind` variant, matching `Enum.TryParse`'s acceptance of
/// any enum member name (so `<type>Lyricist</type>` resolves, not falling back
/// to the `Actor` default).
fn parse_person_kind(text: &str) -> Option<PersonKind> {
    const KINDS: [PersonKind; 26] = [
        PersonKind::Unknown,
        PersonKind::Actor,
        PersonKind::Director,
        PersonKind::Composer,
        PersonKind::Writer,
        PersonKind::GuestStar,
        PersonKind::Producer,
        PersonKind::Conductor,
        PersonKind::Lyricist,
        PersonKind::Arranger,
        PersonKind::Engineer,
        PersonKind::Mixer,
        PersonKind::Remixer,
        PersonKind::Creator,
        PersonKind::Artist,
        PersonKind::AlbumArtist,
        PersonKind::Author,
        PersonKind::Illustrator,
        PersonKind::Penciller,
        PersonKind::Inker,
        PersonKind::Colorist,
        PersonKind::Letterer,
        PersonKind::CoverArtist,
        PersonKind::Editor,
        PersonKind::Translator,
        PersonKind::Narrator,
    ];
    let text = text.trim();
    KINDS
        .into_iter()
        .find(|k| format!("{k:?}").eq_ignore_ascii_case(text))
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

/// Provider-id URL scanners — port of `MediaBrowser.Common.Providers.ProviderIdParsers`.
pub mod provider_id_parsers {
    const IMDB_MIN_NUMBERS: usize = 7;
    const IMDB_MAX_NUMBERS: usize = 8;
    const IMDB_PREFIX: &str = "tt";

    /// Finds an IMDb id (`tt` + 7–8 digits) anywhere in `text`
    /// (`ProviderIdParsers.TryFindImdbId`).
    #[must_use]
    pub fn try_find_imdb_id(text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut slice: &[char] = &chars;

        while slice.len() >= 2 + IMDB_MIN_NUMBERS {
            let tt_pos = find_subslice(slice, IMDB_PREFIX)?;
            slice = &slice[tt_pos..];

            let mut i = 2;
            let limit = slice.len().min(IMDB_MAX_NUMBERS + 2);
            while i < limit && slice[i].is_ascii_digit() {
                i += 1;
            }

            if (IMDB_MIN_NUMBERS + 2..=IMDB_MAX_NUMBERS + 2).contains(&i) {
                return Some(slice[..i].iter().collect());
            }

            slice = &slice[i..];
        }

        None
    }

    /// Finds a TMDb movie id from a `themoviedb.org/movie/<id>` URL
    /// (`ProviderIdParsers.TryFindTmdbMovieId`).
    #[must_use]
    pub fn try_find_tmdb_movie_id(text: &str) -> Option<String> {
        try_find_provider_id(text, "themoviedb.org/movie/")
    }

    /// Finds a TMDb series id from a `themoviedb.org/tv/<id>` URL
    /// (`ProviderIdParsers.TryFindTmdbSeriesId`).
    #[must_use]
    pub fn try_find_tmdb_series_id(text: &str) -> Option<String> {
        try_find_provider_id(text, "themoviedb.org/tv/")
    }

    /// Finds a TVDb id from a `thetvdb.com/?tab=series&id=<id>` URL
    /// (`ProviderIdParsers.TryFindTvdbId`).
    #[must_use]
    pub fn try_find_tvdb_id(text: &str) -> Option<String> {
        try_find_provider_id(text, "thetvdb.com/?tab=series&id=")
    }

    /// Scans the digits immediately following `search` in `text`.
    fn try_find_provider_id(text: &str, search: &str) -> Option<String> {
        let idx = text.find(search)?;
        let rest = &text[idx + search.len()..];
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if id.is_empty() { None } else { Some(id) }
    }

    /// Finds the first index of `needle` in `haystack` (char-slice `IndexOf`).
    fn find_subslice(haystack: &[char], needle: &str) -> Option<usize> {
        let needle: Vec<char> = needle.chars().collect();
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        (0..=haystack.len() - needle.len()).find(|&i| haystack[i..i + needle.len()] == needle[..])
    }
}
