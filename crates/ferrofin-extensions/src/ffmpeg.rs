//! The intro skipper's ffmpeg processes — port of the plugin's
//! `FFmpegService` (intro-skipper `db09359`): how every analysis process runs
//! (`GetOutputAsync`/`GetProcessOutputAsync`: `-threads` from
//! `ProcessThreads`, the OS priority from `ProcessPriority`), and the
//! detections besides Chromaprint (silence, keyframes, the audio-stream
//! duration).

use std::process::{Output, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use regex::Regex;

/// A .NET `ProcessPriorityClass` (`ProcessPriority`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// `Idle`.
    Idle,
    /// `BelowNormal` (the default).
    BelowNormal,
    /// `Normal`.
    Normal,
    /// `AboveNormal`.
    AboveNormal,
    /// `High`.
    High,
    /// `RealTime`.
    RealTime,
}

impl Priority {
    /// The nice value .NET sets for the class on Unix
    /// (`Process.SetPriorityClassCore`).
    #[must_use]
    pub fn nice(self) -> i32 {
        match self {
            Self::Idle => 19,
            Self::BelowNormal => 10,
            Self::Normal => 0,
            Self::AboveNormal => -6,
            Self::High => -11,
            Self::RealTime => -19,
        }
    }
}

/// How analysis processes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessOptions {
    /// The OS priority (`ProcessPriority`).
    pub priority: Priority,
    /// `-threads` for ffmpeg (`ProcessThreads`; 0 lets ffmpeg decide).
    pub threads: i32,
}

impl Default for ProcessOptions {
    /// Upstream's defaults: `BelowNormal`, 0 threads.
    fn default() -> Self {
        Self::new("BelowNormal", 0)
    }
}

impl ProcessOptions {
    /// From the plugin's `ProcessPriority` (a .NET `ProcessPriorityClass`, by
    /// name or number; an unknown one is the default, `BelowNormal`) and
    /// `ProcessThreads`.
    #[must_use]
    pub fn new(priority: &str, threads: i32) -> Self {
        let priority = match priority.trim() {
            p if p.eq_ignore_ascii_case("Idle") || p == "64" => Priority::Idle,
            p if p.eq_ignore_ascii_case("Normal") || p == "32" => Priority::Normal,
            p if p.eq_ignore_ascii_case("AboveNormal") || p == "32768" => Priority::AboveNormal,
            p if p.eq_ignore_ascii_case("High") || p == "128" => Priority::High,
            p if p.eq_ignore_ascii_case("RealTime") || p == "256" => Priority::RealTime,
            _ => Priority::BelowNormal,
        };
        Self { priority, threads }
    }
}

/// Runs `program args…` at the options' priority, capturing its output; an
/// error only when it could not be spawned. The exit status is the caller's
/// to judge (upstream reads whatever the process wrote).
pub(crate) async fn output(
    program: &str,
    args: &[&str],
    options: ProcessOptions,
) -> Result<Output, String> {
    tracing::debug!(program, ?args, "intro skipper: starting process");
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The analysis cancels by dropping this future; the process (minutes
        // of CPU per file) must die with it.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    set_priority(&child, options.priority);
    child
        .wait_with_output()
        .await
        .map_err(|e| format!("{program}: {e}"))
}

/// `process.PriorityClass = …`, right after the start as upstream: a
/// failure (raising the priority needs privileges) leaves the process as it
/// is, with one warning per run of the server rather than one per process.
fn set_priority(child: &tokio::process::Child, priority: Priority) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if let Err(error) = apply_priority(child, priority)
        && !WARNED.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(
            ?priority,
            %error,
            "intro skipper: ffmpeg priority could not be modified (ProcessPriority)"
        );
    }
}

/// .NET's Unix `SetPriorityClassCore`: the class's nice value.
#[cfg(unix)]
fn apply_priority(child: &tokio::process::Child, priority: Priority) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    // SAFETY: setpriority only reads its scalar arguments.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::id_t::from(pid), priority.nice()) } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// .NET's Windows `SetPriorityClassCore`: the class itself.
#[cfg(windows)]
fn apply_priority(child: &tokio::process::Child, priority: Priority) -> std::io::Result<()> {
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS, SetPriorityClass,
    };
    let Some(handle) = child.raw_handle() else {
        return Ok(());
    };
    let class = match priority {
        Priority::Idle => IDLE_PRIORITY_CLASS,
        Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Normal => NORMAL_PRIORITY_CLASS,
        Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        Priority::High => HIGH_PRIORITY_CLASS,
        Priority::RealTime => REALTIME_PRIORITY_CLASS,
    };
    // SAFETY: the handle is the live child's, owned by `child`.
    if unsafe { SetPriorityClass(handle.cast(), class) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Other targets have no priority to set.
#[cfg(not(any(unix, windows)))]
fn apply_priority(_child: &tokio::process::Child, _priority: Priority) -> std::io::Result<()> {
    Ok(())
}

/// `GetOutputAsync`'s ffmpeg command line: `-hide_banner -threads N
/// -loglevel L args…`, at `info` for the filters that report at that level.
fn ffmpeg_args<'a>(args: &[&'a str], threads: &'a str) -> Vec<&'a str> {
    let info = args.iter().any(|a| {
        let a = a.to_ascii_lowercase();
        [
            "silencedetect",
            "blackframe",
            "blackdetect",
            "metadata=print",
            "showinfo",
        ]
        .iter()
        .any(|filter| a.contains(filter))
    });
    let mut out = vec!["-hide_banner", "-threads", threads, "-loglevel"];
    out.push(if info { "info" } else { "warning" });
    out.extend_from_slice(args);
    out
}

/// Runs ffmpeg as `GetOutputAsync` does, returning its output.
pub(crate) async fn ffmpeg(
    ffmpeg: &str,
    args: &[&str],
    options: ProcessOptions,
) -> Result<Output, String> {
    let threads = options.threads.to_string();
    output(ffmpeg, &ffmpeg_args(args, &threads), options).await
}

/// The detections the analysis runs through ffmpeg besides Chromaprint
/// (`IFFmpegService`), behind a seam for tests.
#[async_trait]
pub trait FfmpegService: Send + Sync {
    /// `DetectSilenceAsync`: the silences in `[start, end]` of `path` quieter
    /// than `noise` dB, as absolute `(start, end)` seconds.
    async fn detect_silence(
        &self,
        path: &str,
        start: f64,
        end: f64,
        noise: i32,
        options: ProcessOptions,
    ) -> Result<Vec<(f64, f64)>, String>;

    /// `DetectKeyFramesAsync`: the keyframe times in `[start, end]` of `path`.
    async fn detect_keyframes(
        &self,
        path: &str,
        start: f64,
        end: f64,
        options: ProcessOptions,
    ) -> Result<Vec<f64>, String>;

    /// `ProbeAudioDurationAsync`: the first audio stream's duration, seconds.
    async fn probe_audio_duration(&self, path: &str, options: ProcessOptions) -> Option<f64>;
}

/// The real [`FfmpegService`] over the server's ffmpeg and ffprobe.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    ffmpeg: String,
    ffprobe: String,
}

impl Ffmpeg {
    /// Uses these binaries.
    #[must_use]
    pub fn new(ffmpeg: impl Into<String>, ffprobe: impl Into<String>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
        }
    }
}

#[async_trait]
impl FfmpegService for Ffmpeg {
    async fn detect_silence(
        &self,
        path: &str,
        start: f64,
        end: f64,
        noise: i32,
        options: ProcessOptions,
    ) -> Result<Vec<(f64, f64)>, String> {
        let (from, to) = (start.to_string(), (end - start).to_string());
        let filter = format!("silencedetect=noise={noise}dB:duration=0.1");
        let out = ffmpeg(
            &self.ffmpeg,
            &[
                "-vn", "-sn", "-dn", "-ss", &from, "-i", path, "-to", &to, "-af", &filter, "-f",
                "null", "-",
            ],
            options,
        )
        .await?;
        Ok(parse_silence(&String::from_utf8_lossy(&out.stderr), start))
    }

    async fn detect_keyframes(
        &self,
        path: &str,
        start: f64,
        end: f64,
        options: ProcessOptions,
    ) -> Result<Vec<f64>, String> {
        let (from, to) = (start.to_string(), (end - start).to_string());
        let out = ffmpeg(
            &self.ffmpeg,
            &[
                "-skip_frame",
                "nokey",
                "-ss",
                &from,
                "-i",
                path,
                "-to",
                &to,
                "-an",
                "-dn",
                "-sn",
                "-vf",
                "showinfo",
                "-f",
                "null",
                "-",
            ],
            options,
        )
        .await?;
        Ok(parse_keyframes(
            &String::from_utf8_lossy(&out.stderr),
            start,
        ))
    }

    async fn probe_audio_duration(&self, path: &str, options: ProcessOptions) -> Option<f64> {
        let out = output(
            &self.ffprobe,
            &[
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=duration:stream_tags=DURATION",
                "-of",
                "csv=p=0",
                path,
            ],
            options,
        )
        .await
        .inspect_err(
            |err| tracing::debug!(%err, path, "intro skipper: audio duration probe failed"),
        )
        .ok()?;
        parse_audio_duration(&String::from_utf8_lossy(&out.stdout))
    }
}

/// `FFmpegOutputParser`'s silence expression, verbatim.
static SILENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"silence_(?<type>start|end): (?<time>[0-9\.]+)").expect("silence regex")
});

/// `ParseSilence`: each `silence_end` closes the last `silence_start`; times
/// are relative to the window start.
fn parse_silence(raw: &str, range_start: f64) -> Vec<(f64, f64)> {
    let mut current = (0.0, 0.0);
    let mut ranges = Vec::new();
    for found in SILENCE.captures_iter(raw) {
        // Upstream's `Convert.ToDouble` would throw on a malformed number
        // (the regex admits "1.2.3") and drop the whole result; only that
        // match is skipped here.
        let Ok(time) = found["time"].parse::<f64>() else {
            continue;
        };
        if &found["type"] == "start" {
            current.0 = time + range_start;
        } else {
            current.1 = time + range_start;
            ranges.push(current);
        }
    }
    ranges
}

/// `ParseKeyFrames`: the `pts_time:` of each `showinfo` line.
fn parse_keyframes(raw: &str, range_start: f64) -> Vec<f64> {
    raw.split('\n')
        .filter_map(|line| {
            let index = line.to_ascii_lowercase().find("pts_time:")?;
            let value = line[index + 9..].split(' ').next()?;
            if let Ok(time) = value.parse::<f64>() {
                Some(time + range_start)
            } else {
                tracing::debug!(value, line, "intro skipper: unparsable keyframe time");
                None
            }
        })
        .collect()
}

/// `ProbeAudioDurationAsync`'s parse: the first positive field of the first
/// line, as seconds or as a `[d.]hh:mm:ss[.f]` duration (Matroska's
/// `DURATION` tag; parsed whatever its fraction length — .NET's
/// `TimeSpan.TryParse` stops at seven digits, which would reject the nine
/// Matroska writes).
fn parse_audio_duration(output: &str) -> Option<f64> {
    output
        .trim()
        .lines()
        .next()?
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("N/A"))
        .find_map(|v| {
            v.parse::<f64>()
                .ok()
                .or_else(|| timespan_seconds(v))
                .filter(|s| *s > 0.0)
        })
}

/// `[d.]hh:mm:ss[.f]` as seconds.
fn timespan_seconds(value: &str) -> Option<f64> {
    let mut parts = value.split(':');
    let (head, minutes, seconds) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (days, hours) = match head.split_once('.') {
        Some((d, h)) => (d.parse::<f64>().ok()?, h.parse::<f64>().ok()?),
        None => (0.0, head.parse::<f64>().ok()?),
    };
    let (minutes, seconds) = (minutes.parse::<f64>().ok()?, seconds.parse::<f64>().ok()?);
    Some(((days * 24.0 + hours) * 60.0 + minutes) * 60.0 + seconds)
}

/// A canned [`FfmpegService`] for tests, counting its detections.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeFfmpeg {
    pub silences: Vec<(f64, f64)>,
    pub keyframes: Vec<f64>,
    pub audio_duration: Option<f64>,
    pub calls: std::sync::atomic::AtomicUsize,
    pub log: std::sync::Mutex<Vec<FakeCall>>,
}

/// A [`FakeFfmpeg`] detection: `(kind, path, start, end, noise, options)`.
#[cfg(test)]
pub(crate) type FakeCall = (&'static str, String, f64, f64, i32, ProcessOptions);

#[cfg(test)]
impl FakeFfmpeg {
    fn record(&self, call: (&'static str, &str, f64, f64, i32, ProcessOptions)) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut log) = self.log.lock() {
            log.push((call.0, call.1.to_owned(), call.2, call.3, call.4, call.5));
        }
    }
}

#[cfg(test)]
#[async_trait]
impl FfmpegService for FakeFfmpeg {
    async fn detect_silence(
        &self,
        path: &str,
        start: f64,
        end: f64,
        noise: i32,
        options: ProcessOptions,
    ) -> Result<Vec<(f64, f64)>, String> {
        self.record(("silence", path, start, end, noise, options));
        Ok(self.silences.clone())
    }

    async fn detect_keyframes(
        &self,
        path: &str,
        start: f64,
        end: f64,
        options: ProcessOptions,
    ) -> Result<Vec<f64>, String> {
        self.record(("keyframes", path, start, end, 0, options));
        Ok(self.keyframes.clone())
    }

    async fn probe_audio_duration(&self, _path: &str, _options: ProcessOptions) -> Option<f64> {
        self.audio_duration
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact decimal parses
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case("BelowNormal", 10)]
    #[case("idle", 19)]
    #[case("Normal", 0)]
    #[case("AboveNormal", -6)]
    #[case("High", -11)]
    #[case("RealTime", -19)]
    #[case("16384", 10)]
    #[case("64", 19)]
    #[case("bogus", 10)]
    fn priority_classes_map_to_dotnet_nice_values(#[case] priority: &str, #[case] nice: i32) {
        assert_eq!(ProcessOptions::new(priority, 0).priority.nice(), nice);
    }

    #[test]
    fn ffmpeg_args_follow_get_output_async() {
        assert_eq!(
            ffmpeg_args(&["-i", "x", "-af", "silencedetect=noise=-50dB"], "4"),
            [
                "-hide_banner",
                "-threads",
                "4",
                "-loglevel",
                "info",
                "-i",
                "x",
                "-af",
                "silencedetect=noise=-50dB"
            ]
        );
        assert_eq!(
            ffmpeg_args(&["-i", "x", "-f", "chromaprint"], "0")[..5],
            ["-hide_banner", "-threads", "0", "-loglevel", "warning"]
        );
    }

    #[test]
    fn silence_pairs_follow_parse_silence() {
        let raw = "[silencedetect @ 0x0] silence_start: 12.34\n\
                   [silencedetect @ 0x0] silence_end: 56.123 | silence_duration: 43.783\n\
                   [silencedetect @ 0x0] silence_start: 60\n";
        assert_eq!(parse_silence(raw, 100.0), [(112.34, 156.123)]);
    }

    #[test]
    fn keyframes_follow_parse_key_frames() {
        let raw = "[Parsed_showinfo_0 @ 0x0] n:   0 pts:  0 pts_time:0 duration: 1\n\
                   noise\n\
                   [Parsed_showinfo_0 @ 0x0] n:   1 pts:  1 pts_time:2.5 duration: 1\n\
                   [Parsed_showinfo_0 @ 0x0] n:   2 pts:  1 PTS_TIME:bad x\n";
        assert_eq!(parse_keyframes(raw, 10.0), [10.0, 12.5]);
    }

    #[rstest::rstest]
    #[case("1402.5\n", Some(1402.5))]
    #[case("N/A,00:23:22.500000000\n", Some(1402.5))]
    #[case("1.00:00:01\n", Some(86_401.0))]
    #[case("N/A,N/A\n", None)]
    #[case("0,\n", None)]
    #[case("", None)]
    fn audio_durations_follow_the_probe(#[case] output: &str, #[case] seconds: Option<f64>) {
        assert_eq!(parse_audio_duration(output), seconds);
    }

    /// The real processes against a generated clip: 3 s of tone, 2 s of
    /// silence, 3 s of tone, with a keyframe every second. Gated like the
    /// other ffmpeg integration tests.
    #[tokio::test]
    async fn real_ffmpeg_detections() {
        if std::env::var_os("FERROFIN_FFMPEG_TESTS").is_none() {
            eprintln!("skipped: set FERROFIN_FFMPEG_TESTS=1 to run");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let clip = dir.path().join("clip.mkv");
        let clip = clip.to_str().expect("utf-8");
        let made = output(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=8,volume=enable='between(t,3,5)':volume=0",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10:duration=8",
                "-c:v",
                "libx264",
                "-g",
                "10",
                "-c:a",
                "aac",
                "-shortest",
                clip,
            ],
            ProcessOptions::default(),
        )
        .await
        .expect("ffmpeg");
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let service = Ffmpeg::new("ffmpeg", "ffprobe");
        let options = ProcessOptions::new("Idle", 1);
        let silences = service
            .detect_silence(clip, 1.0, 7.0, -50, options)
            .await
            .expect("silence");
        assert!(
            silences
                .iter()
                .any(|(s, e)| (*s - 3.0).abs() < 0.2 && (*e - 5.0).abs() < 0.2),
            "{silences:?}"
        );
        let keyframes = service
            .detect_keyframes(clip, 2.0, 6.0, options)
            .await
            .expect("keyframes");
        assert!(
            keyframes.iter().any(|k| (*k - 4.0).abs() < 0.05),
            "{keyframes:?}"
        );
        let audio = service
            .probe_audio_duration(clip, options)
            .await
            .expect("duration");
        assert!((audio - 8.0).abs() < 0.2, "{audio}");
    }

    /// The process runs at the priority's nice value (lowering it needs no
    /// privileges). Read back from `/proc` once the priority is set.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn processes_run_at_the_configured_priority() {
        let out = output(
            "sh",
            &["-c", "sleep 0.3; cut -d' ' -f19 /proc/$$/stat"],
            ProcessOptions::new("Idle", 0),
        )
        .await
        .expect("sh");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "19");
    }
}
