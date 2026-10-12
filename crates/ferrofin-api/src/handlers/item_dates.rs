//! ItemsController's DateTime query binding followed by ToUniversalTime.
//!
//! ASP.NET uses AdjustToUniversal | AllowWhiteSpaces, without AssumeUniversal:
//! an offsetless value remains Unspecified until the controller converts it
//! using the server's local time zone. LiveTV has a separate existing binder.

use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike,
    Utc,
};
use serde::{Deserialize, Deserializer};

pub(super) fn deserialize_optional_premiere_date<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    let Some(raw) = raw.as_deref().map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    parse_premiere_date(raw)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("invalid premiere date"))
}

fn date_limits() -> Option<(NaiveDateTime, NaiveDateTime)> {
    let min = NaiveDate::from_ymd_opt(1, 1, 1)?.and_hms_opt(0, 0, 0)?;
    let max = NaiveDate::from_ymd_opt(9999, 12, 31)?.and_hms_nano_opt(23, 59, 59, 999_999_900)?;
    Some((min, max))
}

fn parse_premiere_date(raw: &str) -> Option<DateTime<Utc>> {
    let (whole, ticks) = split_fraction(raw)?;
    let (min, max) = date_limits()?;
    if let Ok(aware) = DateTime::parse_from_rfc3339(&whole) {
        if aware.offset().local_minus_utc().unsigned_abs() > 14 * 3600
            || aware.nanosecond() >= 1_000_000_000
        {
            return None;
        }
        let local = aware
            .naive_local()
            .checked_add_signed(Duration::nanoseconds(ticks * 100))?;
        if !(min..=max).contains(&local) {
            return None;
        }
        let mut utc = local.checked_sub_signed(Duration::seconds(i64::from(
            aware.offset().local_minus_utc(),
        )))?;
        // DateTimeParse.AdjustTimeZoneToUniversal rolls a lower-bound offset
        // underflow forward one day; it rejects upper-bound overflow. Local
        // ToUniversalTime below instead clamps at either boundary.
        if utc < min {
            utc = utc.checked_add_signed(Duration::days(1))?;
        }
        return (min..=max).contains(&utc).then(|| utc.and_utc());
    }
    let local = ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(&whole, format).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(&whole, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
        })?;
    if local.nanosecond() >= 1_000_000_000 {
        return None;
    }
    let local = local.checked_add_signed(Duration::nanoseconds(ticks * 100))?;
    if !(min..=max).contains(&local) {
        return None;
    }
    let raw_offset = match Local.from_local_datetime(&local) {
        LocalResult::Single(value) => i64::from(value.offset().local_minus_utc()),
        // TimeZoneInfoOptions.NoThrowOnInvalidTime uses the standard offset
        // for both folds and gaps, including negative and half-hour DST.
        LocalResult::Ambiguous(_, _) | LocalResult::None => standard_local_offset(local)?,
    };
    // TimeZoneInfo's TZif adjustment deltas have minute granularity. Round
    // relative to its current standard base, not the historical raw offset.
    let base = standard_local_offset(Utc::now().naive_utc())? / 60 * 60;
    let offset = base + (raw_offset - base) / 60 * 60;
    let utc = local
        .checked_sub_signed(Duration::seconds(offset))?
        .clamp(min, max);
    Some(utc.and_utc())
}

/// DateTimeParse.ParseFraction accumulates each decimal digit as a double and
/// rounds to 100ns with Math.Round (ties to even); the floating accumulation
/// matters, e.g. .00000025 becomes three ticks rather than two.
#[allow(clippy::cast_possible_truncation)] // Rounded finite fraction is bounded to 0..=10_000_000 ticks.
fn split_fraction(raw: &str) -> Option<(String, i64)> {
    let Some(index) = raw.find('.') else {
        return Some((raw.to_owned(), 0));
    };
    let digits = raw[index + 1..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    let mut fraction = 0.0_f64;
    let mut place = 0.1_f64;
    for digit in raw[index + 1..index + 1 + digits].bytes() {
        fraction += f64::from(digit - b'0') * place;
        place *= 0.1;
    }
    let ticks = (fraction * 10_000_000.0).round_ties_even() as i64;
    Some((
        format!("{}{}", &raw[..index], &raw[index + 1 + digits..]),
        ticks,
    ))
}

#[cfg(unix)]
fn standard_local_offset(local: NaiveDateTime) -> Option<i64> {
    // SAFETY: zero is valid for every libc::tm field (including its optional
    // zone pointer); all input fields are initialized before mktime.
    let mut submitted: libc::tm = unsafe { std::mem::zeroed() };
    submitted.tm_year = local.year() - 1900;
    submitted.tm_mon = i32::try_from(local.month0()).ok()?;
    submitted.tm_mday = i32::try_from(local.day()).ok()?;
    submitted.tm_hour = i32::try_from(local.hour()).ok()?;
    submitted.tm_min = i32::try_from(local.minute()).ok()?;
    submitted.tm_sec = i32::try_from(local.second()).ok()?;
    submitted.tm_isdst = 0;
    // SAFETY: mktime owns neither pointer and accepts an initialized writable
    // tm. Its normalization during a gap must not replace the submitted wall
    // time when computing the offset.
    let timestamp = unsafe { libc::mktime(&raw mut submitted) };
    // -1 is also a valid timestamp. Verify the normalized tm against a
    // reentrant reverse conversion instead of treating -1 as an error.
    // SAFETY: both objects are fully initialized and the pointers remain valid
    // for localtime_r; it returns null when time_t cannot represent the time.
    let mut normalized: libc::tm = unsafe { std::mem::zeroed() };
    let valid = unsafe { libc::localtime_r(&raw const timestamp, &raw mut normalized) };
    if valid.is_null()
        || (
            normalized.tm_year,
            normalized.tm_mon,
            normalized.tm_mday,
            normalized.tm_hour,
            normalized.tm_min,
            normalized.tm_sec,
        ) != (
            submitted.tm_year,
            submitted.tm_mon,
            submitted.tm_mday,
            submitted.tm_hour,
            submitted.tm_min,
            submitted.tm_sec,
        )
    {
        return None;
    }
    // time_t is i32 on some Unix targets and i64 on the supported Linux targets.
    #[allow(clippy::unnecessary_cast, clippy::cast_lossless)]
    let timestamp = timestamp as i64;
    Some(local.and_utc().timestamp() - timestamp)
}

#[cfg(windows)]
fn standard_local_offset(_local: NaiveDateTime) -> Option<i64> {
    let mut seconds = 0;
    // SAFETY: the CRT writes one initialized c_long. tzset initializes the
    // process-local CRT timezone; no environment variable is modified.
    unsafe {
        libc::tzset();
        (libc::get_timezone(&raw mut seconds) == 0).then_some(-i64::from(seconds))
    }
}

#[cfg(not(any(unix, windows)))]
fn standard_local_offset(_local: NaiveDateTime) -> Option<i64> {
    Some(i64::from(Local::now().offset().local_minus_utc()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premiere_dates_explicit_offsets_keep_source_fraction_and_boundaries() {
        for (raw, expected) in [
            (
                "2026-01-02T03:04:05.0000001+02:00",
                "2026-01-02T01:04:05.0000001Z",
            ),
            (
                "2026-01-02T03:04:05.00000015Z",
                "2026-01-02T03:04:05.0000002Z",
            ),
            (
                "2026-01-02T03:04:05.00000025Z",
                "2026-01-02T03:04:05.0000003Z",
            ),
            ("2026-01-02T23:59:59.99999995Z", "2026-01-03T00:00:00Z"),
            ("0001-01-01T00:00:00+02:00", "0001-01-01T22:00:00Z"),
        ] {
            assert_eq!(
                parse_premiere_date(raw),
                Some(expected.parse().unwrap()),
                "{raw}"
            );
        }
        for raw in [
            "0000-01-01T00:00:00Z",
            "9999-12-31T23:59:59.99999995Z",
            "9999-12-31T23:59:59-02:00",
            "2026-02-30",
            "2026-01-02T03:04:05+14:01",
            "2016-12-31T23:59:60Z",
            "2026-01-02T03:04:05.Z",
        ] {
            assert!(parse_premiere_date(raw).is_none(), "{raw}");
        }
    }
}
