//! .NET tick units (`TimeSpan.TicksPer*`, `DateTime.UnixEpoch.Ticks`).
//!
//! A tick is 100 nanoseconds. Jellyfin stores every run time, position and
//! chapter start in ticks, and its `DateTime.Ticks` count from
//! `0001-01-01T00:00:00`.

/// `TimeSpan.TicksPerMillisecond`.
pub const TICKS_PER_MS: i64 = 10_000;
/// `TimeSpan.TicksPerSecond`.
pub const TICKS_PER_SECOND: i64 = 1_000 * TICKS_PER_MS;
/// `TimeSpan.TicksPerMinute`.
pub const TICKS_PER_MINUTE: i64 = 60 * TICKS_PER_SECOND;
/// `TimeSpan.TicksPerHour`.
pub const TICKS_PER_HOUR: i64 = 60 * TICKS_PER_MINUTE;
/// `TimeSpan.TicksPerDay`.
pub const TICKS_PER_DAY: i64 = 24 * TICKS_PER_HOUR;
/// [`TICKS_PER_SECOND`] as an `f64`, for `TimeSpan.TotalSeconds`-style
/// conversions (an `i64 as f64` cast here would trip
/// `clippy::cast_precision_loss`).
pub const TICKS_PER_SECOND_F64: f64 = 10_000_000.0;
/// `DateTime.UnixEpoch.Ticks`: ticks from `0001-01-01` to `1970-01-01`.
pub const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_match_dotnet() {
        assert_eq!(TICKS_PER_MS, 10_000);
        assert_eq!(TICKS_PER_SECOND, 10_000_000);
        assert_eq!(TICKS_PER_MINUTE, 600_000_000);
        assert_eq!(TICKS_PER_HOUR, 36_000_000_000);
        assert_eq!(TICKS_PER_DAY, 864_000_000_000);
        let per_second = u32::try_from(TICKS_PER_SECOND).expect("fits u32");
        assert!((TICKS_PER_SECOND_F64 - f64::from(per_second)).abs() < f64::EPSILON);
        // 719_162 days from 0001-01-01 to 1970-01-01.
        assert_eq!(UNIX_EPOCH_TICKS, 719_162 * TICKS_PER_DAY);
    }
}
