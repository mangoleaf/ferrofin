//! `FfProbe` based keyframe extractor.
//!
//! Port of `Jellyfin.MediaEncoding.Keyframes.FfProbe.FfProbeKeyframeExtractor`.

use std::io::{BufRead, Read};
use std::process::{Command, Stdio};

use crate::error::KeyframesError;
use crate::keyframe_data::KeyframeData;

/// The number of ticks in a second. 1 tick = 100ns (mirrors `TimeSpan.TicksPerSecond`).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The number of ticks in a millisecond (mirrors `TimeSpan.TicksPerMillisecond`).
const TICKS_PER_MILLISECOND: i64 = 10_000;

/// Extracts the keyframes using the ffprobe executable at the specified path.
///
/// # Arguments
///
/// * `ff_probe_path` - The path to the ffprobe executable.
/// * `file_path` - The file path.
///
/// # Errors
///
/// Returns [`KeyframesError`] if the ffprobe process cannot be spawned or its
/// standard output cannot be read.
pub fn get_keyframe_data(
    ff_probe_path: &str,
    file_path: &str,
) -> Result<KeyframeData, KeyframesError> {
    let mut process = Command::new(ff_probe_path)
        .args([
            "-fflags",
            "+genpts",
            "-v",
            "error",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "format=duration",
            "-show_entries",
            "stream=duration",
            "-show_entries",
            "packet=pts_time,flags",
            "-select_streams",
            "v",
            "-of",
            "csv",
            file_path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture ffprobe stdout"))?;

    let result = parse_stream(stdout);

    // Reap the child regardless of parse outcome.
    let _ = process.wait();

    Ok(result)
}

/// Parses the CSV stream produced by ffprobe into [`KeyframeData`].
///
/// This is the core, process-free logic and the sole parity test target.
#[must_use]
pub fn parse_stream<R: Read>(reader: R) -> KeyframeData {
    let mut keyframes: Vec<i64> = Vec::new();
    let mut stream_duration: f64 = 0.0;
    let mut format_duration: f64 = 0.0;

    let buf = std::io::BufReader::new(reader);
    for line in buf.lines() {
        // Skip lines that cannot be read (mirrors treating them as unusable).
        let Ok(line) = line else { continue };
        if line.is_empty() {
            continue;
        }

        let Some(first_comma) = line.find(',') else {
            continue;
        };
        let line_type = &line[..first_comma];
        let rest = &line[first_comma + 1..];

        if line_type.eq_ignore_ascii_case("packet") {
            // Split time and flags from the packet line. Example line: packet,7169.079000,K_
            let Some(second_comma) = rest.find(',') else {
                continue;
            };
            let pts_time = &rest[..second_comma];
            let flags = &rest[second_comma + 1..];
            if flags.starts_with("K_")
                && let Ok(keyframe) = pts_time.parse::<f64>()
            {
                // Have to manually convert to ticks to avoid rounding errors as
                // TimeSpan is only precise down to 1 ms when converting double.
                // The `i64 as f64` mirrors C# promoting `TimeSpan.TicksPerSecond`
                // (a long) to double for the multiplication.
                #[allow(clippy::cast_precision_loss)]
                let ticks = keyframe * TICKS_PER_SECOND as f64;
                keyframes.push(convert_to_i64(ticks));
            }
        } else if line_type.eq_ignore_ascii_case("stream") {
            if let Ok(stream_duration_result) = rest.parse::<f64>() {
                stream_duration = stream_duration_result;
            }
        } else if line_type.eq_ignore_ascii_case("format")
            && let Ok(format_duration_result) = rest.parse::<f64>()
        {
            format_duration = format_duration_result;
        }
    }

    // Prefer the stream duration as it should be more accurate.
    let duration = if stream_duration > 0.0 {
        stream_duration
    } else {
        format_duration
    };

    KeyframeData::new(time_span_from_seconds_ticks(duration), keyframes)
}

/// Mirrors C# `Convert.ToInt64(double)`: round-half-to-even (banker's rounding).
fn convert_to_i64(value: f64) -> i64 {
    // `f64::round_ties_even` rounds halves to the nearest even integer, matching
    // .NET's `Convert.ToInt64` rounding behaviour. The truncating cast is the
    // intended semantics (parity with `Convert.ToInt64(double)`).
    #[allow(clippy::cast_possible_truncation)]
    let ticks = value.round_ties_even() as i64;
    ticks
}

/// Mirrors C# `TimeSpan.FromSeconds(value).Ticks`.
///
/// `TimeSpan.FromSeconds` rounds the value to the nearest millisecond, then the
/// resulting whole-millisecond span is expressed in 100ns ticks.
fn time_span_from_seconds_ticks(value: f64) -> i64 {
    let millis = value * 1000.0;
    let rounded_millis = convert_to_i64(millis);
    rounded_millis * TICKS_PER_MILLISECOND
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_stream branch coverage -------------------------------------

    #[test]
    fn parse_stream_skips_empty_and_malformed_lines() {
        // Line 1: empty (skipped, line 83).
        // Line 2: no comma at all (skipped, line 87).
        // Line 3: a packet line missing the flags field, i.e. no second comma
        //         (skipped, line 95).
        // Line 4: a valid keyframe so we can assert the good path still runs.
        let input = "\n\
             nocommahere\n\
             packet,1.0\n\
             packet,1.000000,K_\n";
        let data = parse_stream(input.as_bytes());
        // Only the last line yields a keyframe: 1.0s * 10_000_000 = 10_000_000 ticks.
        assert_eq!(data.keyframe_ticks, vec![10_000_000]);
        // No stream/format duration lines → duration 0 → total_duration 0.
        assert_eq!(data.total_duration, 0);
    }

    #[test]
    fn parse_stream_ignores_non_keyframe_and_unparseable_numbers() {
        // Non-K_ flag packet is ignored; unparseable pts_time is ignored;
        // stream/format durations parse and stream wins.
        let input = "packet,1.0,__\n\
             packet,notanumber,K_\n\
             stream,12.5\n\
             format,99.0\n";
        let data = parse_stream(input.as_bytes());
        assert!(data.keyframe_ticks.is_empty());
        // stream_duration 12.5s > 0 → used over format. 12.5s → 12500 ms → 125_000_000 ticks.
        assert_eq!(data.total_duration, 125_000_000);
    }

    #[test]
    fn parse_stream_falls_back_to_format_duration() {
        // No stream line (or non-positive) → format duration is used.
        let input = "format,2.0\n";
        let data = parse_stream(input.as_bytes());
        // 2.0s → 2000 ms → 20_000_000 ticks.
        assert_eq!(data.total_duration, 20_000_000);
    }

    #[test]
    fn parse_stream_case_insensitive_line_types() {
        // OrdinalIgnoreCase match on the line type (mirrors upstream).
        let input = "PACKET,1.0,K_\nSTREAM,1.0\n";
        let data = parse_stream(input.as_bytes());
        assert_eq!(data.keyframe_ticks, vec![10_000_000]);
        assert_eq!(data.total_duration, 10_000_000);
    }

    // --- helper unit tests ------------------------------------------------

    #[test]
    fn convert_to_i64_uses_bankers_rounding() {
        // Round-half-to-even: 0.5 → 0, 1.5 → 2, 2.5 → 2, 3.5 → 4.
        assert_eq!(convert_to_i64(0.5), 0);
        assert_eq!(convert_to_i64(1.5), 2);
        assert_eq!(convert_to_i64(2.5), 2);
        assert_eq!(convert_to_i64(3.5), 4);
        assert_eq!(convert_to_i64(-1.5), -2);
    }

    #[test]
    fn time_span_from_seconds_rounds_to_millisecond() {
        // 1.2345s → 1234.5 ms → banker's round to 1234 ms → 12_340_000 ticks.
        assert_eq!(time_span_from_seconds_ticks(1.2345), 12_340_000);
        // 0 → 0.
        assert_eq!(time_span_from_seconds_ticks(0.0), 0);
    }

    // --- get_keyframe_data process path -----------------------------------

    /// Writes an executable POSIX shell script that prints `payload` to stdout,
    /// so we can drive [`get_keyframe_data`]'s real spawn/read/parse/wait path
    /// without depending on a real ffprobe binary.
    #[cfg(unix)]
    fn write_fake_ffprobe(dir: &std::path::Path, payload: &str) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("fake_ffprobe.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        // Ignore all args; just emit the canned CSV.
        write!(f, "#!/bin/sh\ncat <<'EOF'\n{payload}\nEOF\n").unwrap();
        f.flush().unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn get_keyframe_data_spawns_and_parses() {
        // A per-test unique temp dir (auto-removed on drop) — keying on
        // `process::id()` alone collides between tests sharing this binary under
        // nextest's concurrent runner.
        let dir = tempfile::tempdir().unwrap();
        let script = write_fake_ffprobe(dir.path(), "packet,1.0,K_\nstream,1.0");

        let data = get_keyframe_data(script.to_str().unwrap(), "/does/not/matter.mkv")
            .expect("fake ffprobe should succeed");
        assert_eq!(data.keyframe_ticks, vec![10_000_000]);
        assert_eq!(data.total_duration, 10_000_000);
    }

    #[test]
    fn get_keyframe_data_errors_when_binary_missing() {
        let err = get_keyframe_data(
            "/nonexistent/path/to/ffprobe-should-not-exist",
            "/tmp/whatever.mkv",
        )
        .expect_err("spawning a missing binary must fail");
        // The spawn IO error is surfaced as a KeyframesError.
        assert!(matches!(err, KeyframesError::Process(_)));
    }
}
