//! The ISO-8601 grammar used by `Utf8JsonReader.GetDateTime`.
use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Utc};

fn digits(bytes: &[u8], start: usize, len: usize) -> Option<u32> {
    bytes.get(start..start + len)?.iter().try_fold(0, |n, b| {
        b.is_ascii_digit().then(|| n * 10 + u32::from(b - b'0'))
    })
}

pub(super) fn parse(text: &str) -> Result<DateTime<Utc>, &'static str> {
    parse_iso(text.as_bytes()).ok_or("expected a System.Text.Json ISO-8601 date")
}

fn parse_iso(b: &[u8]) -> Option<DateTime<Utc>> {
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = i32::try_from(digits(b, 0, 4)?).ok()?;
    if year == 0 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year, digits(b, 5, 2)?, digits(b, 8, 2)?)?;
    if b.len() == 10 {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
    }
    if b.get(10) != Some(&b'T') || b.get(13) != Some(&b':') {
        return None;
    }
    let hour = digits(b, 11, 2)?;
    let minute = digits(b, 14, 2)?;
    let mut pos = 16;
    let mut second = 0;
    let mut ticks = 0;
    if b.get(pos) == Some(&b':') {
        second = digits(b, pos + 1, 2)?;
        pos += 3;
        if b.get(pos) == Some(&b'.') {
            pos += 1;
            let start = pos;
            while b.get(pos).is_some_and(u8::is_ascii_digit) {
                if pos - start < 7 {
                    ticks = ticks * 10 + u32::from(b[pos] - b'0');
                }
                pos += 1;
            }
            let count = pos - start;
            if count > 16 {
                return None;
            }
            for _ in count..7 {
                ticks *= 10;
            }
        }
    }
    if second > 59 {
        return None;
    }
    let time = date.and_hms_nano_opt(hour, minute, second, ticks * 100)?;
    if pos == b.len() {
        return Some(time.and_utc());
    }
    if b.get(pos..) == Some(b"Z") {
        return Some(time.and_utc());
    }
    let sign = match b.get(pos) {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    let offset_hour = digits(b, pos + 1, 2)?;
    pos += 3;
    let mut offset_minute = 0;
    if b.get(pos) == Some(&b':') {
        offset_minute = digits(b, pos + 1, 2)?;
        pos += 3;
    }
    if pos != b.len()
        || offset_hour > 14
        || offset_minute > 59
        || (offset_hour == 14 && offset_minute != 0)
    {
        return None;
    }
    let seconds = i32::try_from(offset_hour * 3600 + offset_minute * 60).ok()? * sign;
    FixedOffset::east_opt(seconds)?
        .from_local_datetime(&time)
        .single()
        .map(|t| t.with_timezone(&Utc))
}
