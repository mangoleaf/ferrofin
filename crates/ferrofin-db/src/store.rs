//! Canonical to-storage formatting for values bound into SQL.
//!
//! Jellyfin's EF Core writes `Guid` columns as **uppercase hyphenated** text
//! (`DC68CD36-0909-4B02-B129-8B1BA641A16A`) and `DateTime` columns as
//! Microsoft.Data.Sqlite's binder formats them, `yyyy-MM-dd HH:mm:ss.FFFFFFF`:
//! space separator, the seven-digit tick fraction with its trailing zeros
//! dropped (and the `.` with them for a whole second), no timezone suffix, UTC
//! by convention. SQLite compares TEXT with the BINARY collation (no `COLLATE`
//! clauses exist in the schema), so Ferrofin must bind byte-identical formats
//! or comparisons silently miss Jellyfin-written rows. Every SQL bind of a
//! [`Uuid`] or [`DateTime`] goes through these helpers; the formats were
//! verified against real Jellyfin 10.11.8 and 12.4 databases (gl-1565).
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
/// `yyyy-MM-dd HH:mm:ss.FFFFFFF`, the format `SqliteValueBinder` gives a
/// `DateTime` (Microsoft.Data.Sqlite 10.0.14, which Jellyfin 12.4 pins). The
/// fraction is the 100-nanosecond tick, cut as .NET's `DateTime` holds it,
/// without its trailing zeros: `2020-01-01 00:00:00`, `… 09.533`,
/// `… 47.7539647`. Trimmed text still sorts as the instants do.
#[must_use]
pub fn datetime_to_db(instant: DateTime<Utc>) -> String {
    let seconds = instant.format("%Y-%m-%d %H:%M:%S");
    let ticks = instant.timestamp_subsec_nanos() / 100;
    if ticks == 0 {
        return seconds.to_string();
    }
    let fraction = format!("{ticks:07}");
    format!("{seconds}.{}", fraction.trim_end_matches('0'))
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
    fn datetime_keeps_a_seven_digit_fraction_without_trailing_zeros() {
        // Mirrors a real Jellyfin-written value: 2026-08-11 17:46:47.7539647
        let instant = Utc
            .with_ymd_and_hms(2026, 8, 11, 17, 46, 47)
            .single()
            .expect("valid instant")
            + chrono::Duration::nanoseconds(753_964_700);
        assert_eq!(datetime_to_db(instant), "2026-08-11 17:46:47.7539647");
    }

    /// .NET's `FFFFFFF`: the tick fraction without trailing zeros, and no
    /// `.` for a whole second (the forms a Jellyfin 12.4 database holds).
    #[rstest::rstest]
    #[case(0, "2026-01-02 03:04:05")]
    #[case(100, "2026-01-02 03:04:05.0000001")]
    #[case(533_000_000, "2026-01-02 03:04:05.533")]
    #[case(447_945_000, "2026-01-02 03:04:05.447945")]
    #[case(500_000_000, "2026-01-02 03:04:05.5")]
    #[case(332_668_000, "2026-01-02 03:04:05.332668")]
    #[case(999_999_999, "2026-01-02 03:04:05.9999999")]
    fn datetime_drops_the_trailing_zeros_of_its_fraction(#[case] nanos: i64, #[case] text: &str) {
        let instant = Utc
            .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
            .single()
            .expect("valid instant")
            + chrono::Duration::nanoseconds(nanos);
        assert_eq!(datetime_to_db(instant), text);
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
            Some("2026-01-02 03:04:05")
        );
    }

    #[test]
    fn stored_datetime_round_trips_through_sqlx_tolerant_decode() {
        // The written format must stay parseable by the `%F %T%.f` fallback
        // sqlx uses when decoding TEXT into chrono types, with a fraction and
        // without one.
        for text in [
            "2026-08-11 17:46:47.7539647",
            "2026-08-11 17:46:47.5",
            "2026-08-11 17:46:47",
        ] {
            let parsed = chrono::NaiveDateTime::parse_from_str(text, "%F %T%.f").expect("parses");
            assert_eq!(datetime_to_db(parsed.and_utc()), text);
        }
    }
}
