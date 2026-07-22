//! Helper methods for working with ffprobe output — port of
//! `MediaBrowser.MediaEncoding.Probing.FFProbeHelpers`.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use super::dtos::InternalMediaInfoResult;

/// A tag dictionary keyed case-insensitively (keys stored lowercased).
///
/// Upstream this is a `Dictionary<string, string?>` with
/// `StringComparer.OrdinalIgnoreCase`. Here the keys are lowercased on insert
/// and every lookup lowercases its key, giving the same case-insensitive
/// semantics without a custom comparer.
pub type CaseInsensitiveTags = HashMap<String, String>;

/// Normalizes an ffprobe result in place, lowercasing all tag keys so that
/// subsequent lookups are case-insensitive.
///
/// Mirrors `FFProbeHelpers.NormalizeFFProbeResult`.
pub fn normalize_ffprobe_result(result: &mut InternalMediaInfoResult) {
    if let Some(format) = result.format.as_mut()
        && let Some(tags) = format.tags.as_mut()
    {
        *tags = convert_dictionary_to_case_insensitive(tags);
    }

    if let Some(streams) = result.streams.as_mut() {
        for stream in streams.iter_mut() {
            if let Some(tags) = stream.tags.as_mut() {
                *tags = convert_dictionary_to_case_insensitive(tags);
            }
        }
    }
}

/// Builds a flattened, lowercased-key view of a raw tag map, dropping null
/// values. This is the representation the normalizer reads from.
#[must_use]
pub fn flatten_tags<S: std::hash::BuildHasher>(
    tags: &HashMap<String, Option<String>, S>,
) -> CaseInsensitiveTags {
    let mut result = CaseInsensitiveTags::with_capacity(tags.len());
    for (key, value) in tags {
        if let Some(value) = value {
            // Match `TryAdd`: first key (case-insensitively) wins.
            result
                .entry(key.to_ascii_lowercase())
                .or_insert_with(|| value.clone());
        }
    }

    result
}

/// Gets a case-insensitive value from a flattened tag map.
#[must_use]
pub fn get_dictionary_value<'a>(tags: &'a CaseInsensitiveTags, key: &str) -> Option<&'a str> {
    tags.get(&key.to_ascii_lowercase()).map(String::as_str)
}

/// Gets an int from a tag dictionary — mirrors
/// `FFProbeHelpers.GetDictionaryNumericValue`.
#[must_use]
pub fn get_dictionary_numeric_value(tags: &CaseInsensitiveTags, key: &str) -> Option<i32> {
    get_dictionary_value(tags, key).and_then(|val| val.trim().parse::<i32>().ok())
}

/// Gets a UTC `DateTime` from a tag dictionary — mirrors
/// `FFProbeHelpers.GetDictionaryDateTime`.
///
/// Accepts full date/date-time strings (assumed UTC) as well as a bare
/// four-digit year (parsed as `yyyy-01-01T00:00:00Z`).
#[must_use]
pub fn get_dictionary_date_time(tags: &CaseInsensitiveTags, key: &str) -> Option<DateTime<Utc>> {
    let val = get_dictionary_value(tags, key)?.trim();
    parse_flexible_date_time(val)
}

/// Parses a date/date-time string the way `DateTime.TryParse` +
/// `TryParseExact("yyyy")` would, assuming universal time.
#[must_use]
pub fn parse_flexible_date_time(val: &str) -> Option<DateTime<Utc>> {
    if val.is_empty() {
        return None;
    }

    // Full RFC 3339 timestamp with an explicit offset.
    if let Ok(dt) = DateTime::parse_from_rfc3339(val) {
        return Some(dt.with_timezone(&Utc));
    }

    // Common ffprobe/tag date-time shapes, assumed to be UTC.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%MZ",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(val, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }

    // Date only (yyyy-MM-dd).
    if let Ok(date) = NaiveDate::parse_from_str(val, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
    }

    // Bare year (yyyy).
    if val.len() == 4
        && let Ok(year) = val.parse::<i32>()
    {
        let date = NaiveDate::from_ymd_opt(year, 1, 1)?;
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
    }

    None
}

/// Converts a raw tag map into a lowercased-key case-insensitive map.
fn convert_dictionary_to_case_insensitive(
    dict: &HashMap<String, Option<String>>,
) -> HashMap<String, Option<String>> {
    let mut result = HashMap::with_capacity(dict.len());
    for (key, value) in dict {
        // Match `Dictionary.TryAdd`: first key (case-insensitively) wins.
        result
            .entry(key.to_ascii_lowercase())
            .or_insert_with(|| value.clone());
    }

    result
}
