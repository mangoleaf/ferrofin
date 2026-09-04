//! Canonical to-storage formatting for values bound into SQL.
//!
//! Jellyfin's EF Core writes `Guid` columns as **uppercase hyphenated** text
//! (`DC68CD36-0909-4B02-B129-8B1BA641A16A`) and `DateTime` columns as
//! `YYYY-MM-DD HH:MM:SS.fffffff` — space separator, seven fractional digits,
//! no timezone suffix, UTC by convention. SQLite compares TEXT with the BINARY
//! collation (no `COLLATE` clauses exist in the schema), so Ferrofin must bind
//! byte-identical formats or lookups silently miss Jellyfin-written rows.
//! Every SQL bind of a [`Uuid`] or [`DateTime`] goes through these helpers;
//! the formats were verified against a real Jellyfin 10.11.8 database (see
//! the schema fixture `tests/data/jellyfin-10.11.8-schema.sql`).
//!
//! The one deliberate exception: `PresentationUniqueKey` (and the composed
//! `UserData.CustomDataKey` strings) use the lowercase un-hyphenated N-format —
//! keep using [`mod@uuid::fmt`]'s `simple()` for those.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Formats a [`Uuid`] the way Jellyfin stores `Guid` columns: uppercase,
/// hyphenated.
#[must_use]
pub fn guid_to_db(id: Uuid) -> String {
    id.hyphenated()
        .encode_upper(&mut Uuid::encode_buffer())
        .to_owned()
}

/// [`guid_to_db`] lifted over `Option`, for nullable `Guid` columns.
#[must_use]
pub fn opt_guid_to_db(id: Option<Uuid>) -> Option<String> {
    id.map(guid_to_db)
}

/// Formats a UTC instant the way Jellyfin (.NET/EF) stores `DateTime` columns:
/// `YYYY-MM-DD HH:MM:SS.fffffff` (seven fractional digits, no timezone).
///
/// chrono has no `%.7f` specifier, so the 100-nanosecond fraction is formatted
/// by hand from the sub-second nanoseconds.
#[must_use]
pub fn datetime_to_db(instant: DateTime<Utc>) -> String {
    format!(
        "{}.{:07}",
        instant.format("%Y-%m-%d %H:%M:%S"),
        instant.timestamp_subsec_nanos() / 100
    )
}

/// [`datetime_to_db`] lifted over `Option`, for nullable `DateTime` columns.
#[must_use]
pub fn opt_datetime_to_db(instant: Option<DateTime<Utc>>) -> Option<String> {
    instant.map(datetime_to_db)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn guid_is_uppercase_hyphenated() {
        let id = Uuid::parse_str("dc68cd36-0909-4b02-b129-8b1ba641a16a").expect("valid uuid");
        assert_eq!(guid_to_db(id), "DC68CD36-0909-4B02-B129-8B1BA641A16A");
    }

    #[test]
    fn opt_guid_maps_through() {
        assert_eq!(opt_guid_to_db(None), None);
        let id = Uuid::from_u128(0xAB);
        assert_eq!(
            opt_guid_to_db(Some(id)).as_deref(),
            Some("00000000-0000-0000-0000-0000000000AB")
        );
    }

    #[test]
    fn datetime_matches_jellyfin_seven_digit_format() {
        // Mirrors a real Jellyfin-written value: 2026-08-11 17:46:47.7539647
        let instant = Utc
            .with_ymd_and_hms(2026, 8, 11, 17, 46, 47)
            .single()
            .expect("valid instant")
            + chrono::Duration::nanoseconds(753_964_700);
        assert_eq!(datetime_to_db(instant), "2026-08-11 17:46:47.7539647");
    }

    #[test]
    fn datetime_zero_fraction_keeps_seven_digits() {
        let instant = Utc
            .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
            .single()
            .expect("valid instant");
        assert_eq!(datetime_to_db(instant), "2026-01-02 03:04:05.0000000");
    }

    #[test]
    fn opt_datetime_maps_through() {
        assert_eq!(opt_datetime_to_db(None), None);
        let instant = Utc
            .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
            .single()
            .expect("valid instant");
        assert_eq!(
            opt_datetime_to_db(Some(instant)).as_deref(),
            Some("2026-01-02 03:04:05.0000000")
        );
    }

    #[test]
    fn stored_datetime_round_trips_through_sqlx_tolerant_decode() {
        // The written format must stay parseable by the `%F %T%.f` fallback
        // sqlx uses when decoding TEXT into chrono types.
        let text = "2026-08-11 17:46:47.7539647";
        let parsed = chrono::NaiveDateTime::parse_from_str(text, "%F %T%.f").expect("parses");
        let instant = parsed.and_utc();
        assert_eq!(datetime_to_db(instant), text);
    }
}
