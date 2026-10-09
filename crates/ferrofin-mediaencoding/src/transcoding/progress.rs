//! Parses ffmpeg's stderr statistics as Jellyfin's `JobLogger` does.

use ferrofin_traits::media_encoding::TranscodingProgress;

/// A parsed stderr report before the job's seek offset is added.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FfmpegProgress {
    /// The output time reported by ffmpeg, in 100 ns ticks.
    pub elapsed_ticks: Option<i64>,
    /// The reported frames per second.
    pub framerate: Option<f32>,
    /// The output byte count, when `size=` contains `kB`.
    pub bytes_transcoded: Option<i64>,
    /// The reported bitrate, with the upstream 1024 multiplier.
    pub bit_rate: Option<i32>,
}

impl FfmpegProgress {
    /// Adds the input seek offset and calculates the completion percentage.
    ///
    /// Upstream only recognizes output time when the runtime is known, and
    /// only reports a line when it contains a framerate or completion value.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "completion percentage is the upstream double calculation"
    )]
    pub fn for_job(
        self,
        runtime_ticks: Option<i64>,
        start_ticks: Option<i64>,
    ) -> Option<TranscodingProgress> {
        let position_ticks = runtime_ticks
            .and(self.elapsed_ticks)
            .map(|ticks| ticks.saturating_add(start_ticks.unwrap_or(0)));
        let percent_complete = position_ticks
            .zip(runtime_ticks)
            .map(|(position, runtime)| 100.0 * position as f64 / runtime as f64);
        (self.framerate.is_some() || percent_complete.is_some()).then_some(TranscodingProgress {
            position_ticks,
            framerate: self.framerate,
            percent_complete,
            bytes_transcoded: self.bytes_transcoded,
            bit_rate: self.bit_rate,
        })
    }
}

/// Parses one stderr statistics line; ordinary diagnostic lines return `None`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the checked, rounded bitrate is bounded to i32 before conversion"
)]
pub fn parse(line: &str) -> Option<FfmpegProgress> {
    let parts: Vec<_> = line.split(' ').collect();
    let mut progress = FfmpegProgress::default();
    for (index, part) in parts.iter().enumerate() {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("fps") {
            let value = if value.is_empty() {
                parts.get(index + 1).copied().unwrap_or("")
            } else {
                value
            };
            progress.framerate = value.parse().ok();
        } else if key.eq_ignore_ascii_case("time") {
            progress.elapsed_ticks = parse_time(value);
        } else if key.eq_ignore_ascii_case("size") {
            let lower = value.to_ascii_lowercase();
            progress.bytes_transcoded = lower
                .contains("kb")
                .then(|| lower.replace("kb", ""))
                .and_then(|size| size.parse::<i64>().ok())
                .and_then(|size| size.checked_mul(1024));
        } else if key.eq_ignore_ascii_case("bitrate") {
            let lower = value.to_ascii_lowercase();
            progress.bit_rate = lower
                .contains("kbits/s")
                .then(|| lower.replace("kbits/s", ""))
                .and_then(|rate| rate.parse::<f32>().ok())
                .map(|rate| (rate * 1024.0).ceil())
                .filter(|rate| {
                    rate.is_finite() && *rate >= i32::MIN as f32 && *rate <= i32::MAX as f32
                })
                .map(|rate| rate as i32);
        }
    }
    (progress.elapsed_ticks.is_some() || progress.framerate.is_some()).then_some(progress)
}

/// Parses ffmpeg's invariant `[days.]hours:minutes:seconds[.fraction]` output.
fn parse_time(value: &str) -> Option<i64> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let parts: Vec<_> = value.split(':').collect();
    let [hours, minutes, seconds] = parts.as_slice() else {
        return None;
    };
    let (days, hours) = match hours.split_once('.') {
        Some((days, hours)) => (days.parse::<i64>().ok()?, hours.parse::<i64>().ok()?),
        None => (0, hours.parse::<i64>().ok()?),
    };
    let minutes = minutes.parse::<i64>().ok()?;
    let (seconds, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let seconds = seconds.parse::<i64>().ok()?;
    if days < 0
        || hours < 0
        || !(0..60).contains(&minutes)
        || !(0..60).contains(&seconds)
        || fraction.len() > 7
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i64>()
            .ok()?
            .checked_mul(10_i64.pow(u32::try_from(7 - fraction.len()).ok()?))?
    };
    let seconds = days
        .checked_mul(24)?
        .checked_add(hours)?
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(60)?
        .checked_add(seconds)?;
    let ticks = seconds.checked_mul(10_000_000)?.checked_add(fraction)?;
    if negative {
        ticks.checked_neg()
    } else {
        Some(ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_report_compact_and_separated_fps_match_upstream() {
        let p = parse(
            "frame=42 fps= 29.97 size=1024kB time=00:01:12.34 bitrate=123.4kbits/s speed=10x",
        )
        .unwrap();
        assert_eq!(p.elapsed_ticks, Some(723_400_000));
        assert_eq!(p.framerate, Some(29.97));
        assert_eq!(p.bytes_transcoded, Some(1_048_576));
        assert_eq!(p.bit_rate, Some(126_362));
        assert_eq!(
            parse("FPS=24 TIME=00:00:01.0000001 SIZE=2KB BITRATE=1KBITS/S").unwrap(),
            FfmpegProgress {
                elapsed_ticks: Some(10_000_001),
                framerate: Some(24.0),
                bytes_transcoded: Some(2048),
                bit_rate: Some(1024)
            }
        );
        // JobLogger does not consume the following token for size/bitrate.
        let p = parse("fps=25 size= 2kB bitrate= 5kbits/s time=N/A").unwrap();
        assert_eq!(p.bytes_transcoded, None);
        assert_eq!(p.bit_rate, None);
        assert_eq!(p.elapsed_ticks, None);
        assert!(parse("Input #0, matroska, from '/media/film.mkv':").is_none());
        assert!(parse("size=12kB bitrate=999kbits/s").is_none());
        let embedded_units = parse("fps=1 size=kB2KB bitrate=kbits/s3KBITS/S").unwrap();
        assert_eq!(embedded_units.bytes_transcoded, Some(2048));
        assert_eq!(embedded_units.bit_rate, Some(3072));
    }

    #[test]
    fn runtime_and_seek_offset_control_completion_reporting() {
        let p = parse("fps=24 time=00:01:00.000 size=1024kB").unwrap();
        let actual = p
            .for_job(Some(600 * 10_000_000), Some(120 * 10_000_000))
            .unwrap();
        assert_eq!(actual.position_ticks, Some(180 * 10_000_000));
        assert_eq!(actual.percent_complete, Some(30.0));
        let actual = p.for_job(None, Some(120 * 10_000_000)).unwrap();
        assert_eq!(actual.position_ticks, None);
        assert_eq!(actual.percent_complete, None);
        let p = parse("time=00:01:00.000").unwrap();
        assert!(p.for_job(None, None).is_none());
        assert_eq!(
            p.for_job(Some(600 * 10_000_000), None)
                .unwrap()
                .percent_complete,
            Some(10.0)
        );
    }

    #[test]
    fn time_parser_accepts_real_ffmpeg_fraction_and_rejects_invalid_times() {
        for (text, expected) in [
            ("00:00:00", 0),
            ("00:00:00.01", 100_000),
            ("00:00:00.0000001", 1),
            ("25:00:00", 900_000_000_000),
            ("1.01:02:03.4", 901_234_000_000),
            ("-00:00:01", -10_000_000),
        ] {
            assert_eq!(parse_time(text), Some(expected), "{text}");
        }
        for text in [
            "N/A",
            "00:60:00",
            "00:00:60",
            "00:00:00.00000001",
            "00:00:x",
            "00:00:-1",
            "99999999999999999999:00:00",
        ] {
            assert_eq!(parse_time(text), None, "{text}");
        }
    }
}
