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

    /// `DetectBlackFramesAsync(episode, range, …)`'s detection: every frame
    /// in `[start, end]` of `path` at least half black under `threshold`,
    /// with times relative to `start` (the caller keeps those at its own
    /// minimum percentage).
    async fn detect_black_frames(
        &self,
        path: &str,
        start: f64,
        end: f64,
        threshold: i32,
        options: ProcessOptions,
    ) -> Result<Vec<BlackFrame>, String>;

    /// `DetectBlackFramesAsync(episode, threshold)`: the black percentage of
    /// every keyframe from `start` to the end of `path`, times relative to
    /// `start`.
    async fn detect_keyframe_black_frames(
        &self,
        path: &str,
        start: f64,
        threshold: i32,
        options: ProcessOptions,
    ) -> Result<Vec<BlackFrame>, String>;

    /// `DetectBlackIntervalsAsync`'s detection: `blackdetect`'s black
    /// intervals in `[start, end]`, in absolute times.
    async fn detect_black_intervals(
        &self,
        path: &str,
        (start, end): (f64, f64),
        (threshold, minimum): (i32, i32),
        options: ProcessOptions,
    ) -> Result<Vec<BlackInterval>, String>;

    /// `DetectKeyframeVisualsAsync`'s detection: each keyframe's luma entropy
    /// and mean saturation in `[start, end]`, times relative to `start` (not
    /// clipped: ffmpeg may report keyframes past `end`).
    async fn detect_keyframe_visuals(
        &self,
        path: &str,
        start: f64,
        end: f64,
        options: ProcessOptions,
    ) -> Result<Vec<KeyframeVisual>, String>;
}

/// A black stretch `blackdetect` reported (`BlackInterval`), seconds.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlackInterval {
    /// Its start.
    pub start: f64,
    /// Its end.
    pub end: f64,
}

/// `BlackInterval.MinimumDetectionDuration`: `blackdetect`'s `d`.
pub const BLACK_INTERVAL_MINIMUM_DURATION: f64 = 0.1;

/// A keyframe's look (`KeyframeVisual`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KeyframeVisual {
    /// Its time, seconds.
    pub time: f64,
    /// The normalised entropy of its luma histogram.
    pub entropy: f64,
    /// Its mean saturation (`SATAVG`, 8-bit scale).
    pub saturation: f64,
}

/// A frame the `blackframe` filter reported (`BlackFrame`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlackFrame {
    /// How much of the frame is black, percent.
    pub percentage: i32,
    /// Its time, seconds.
    pub time: f64,
    /// Its frame number.
    pub frame: i64,
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

    async fn detect_black_frames(
        &self,
        path: &str,
        start: f64,
        end: f64,
        threshold: i32,
        options: ProcessOptions,
    ) -> Result<Vec<BlackFrame>, String> {
        let (from, to) = (start.to_string(), (end - start).to_string());
        let filter = format!("blackframe=amount=50:threshold={threshold}");
        let out = ffmpeg(
            &self.ffmpeg,
            &[
                "-ss", &from, "-i", path, "-to", &to, "-an", "-dn", "-sn", "-vf", &filter, "-f",
                "null", "-",
            ],
            options,
        )
        .await?;
        Ok(parse_black_frames(&String::from_utf8_lossy(&out.stderr)))
    }

    async fn detect_keyframe_black_frames(
        &self,
        path: &str,
        start: f64,
        threshold: i32,
        options: ProcessOptions,
    ) -> Result<Vec<BlackFrame>, String> {
        let from = start.to_string();
        let filter = format!("blackframe=amount=0:threshold={threshold}");
        let out = ffmpeg(
            &self.ffmpeg,
            &[
                "-skip_frame",
                "nokey",
                "-ss",
                &from,
                "-i",
                path,
                "-an",
                "-dn",
                "-sn",
                "-vf",
                &filter,
                "-f",
                "null",
                "-",
            ],
            options,
        )
        .await?;
        Ok(parse_black_frames(&String::from_utf8_lossy(&out.stderr)))
    }

    async fn detect_black_intervals(
        &self,
        path: &str,
        (start, end): (f64, f64),
        (threshold, minimum): (i32, i32),
        options: ProcessOptions,
    ) -> Result<Vec<BlackInterval>, String> {
        let (from, to) = (start.to_string(), (end - start).to_string());
        let filter = format!(
            "blackdetect=d={BLACK_INTERVAL_MINIMUM_DURATION}:pix_th={}:pic_th={}",
            pixel_threshold(threshold),
            picture_threshold(minimum)
        );
        let out = ffmpeg(
            &self.ffmpeg,
            &[
                "-ss",
                &from,
                "-skip_frame",
                "noref",
                "-i",
                path,
                "-to",
                &to,
                "-an",
                "-dn",
                "-sn",
                "-vf",
                &filter,
                "-f",
                "null",
                "-",
            ],
            options,
        )
        .await?;
        Ok(parse_black_intervals(&String::from_utf8_lossy(&out.stderr))
            .into_iter()
            .map(|i| BlackInterval {
                start: i.start + start,
                end: i.end + start,
            })
            .collect())
    }

    async fn detect_keyframe_visuals(
        &self,
        path: &str,
        start: f64,
        end: f64,
        options: ProcessOptions,
    ) -> Result<Vec<KeyframeVisual>, String> {
        let (from, to) = (start.to_string(), (end - start).to_string());
        // `format=yuv420p` keeps both measures on the 8-bit scale their
        // thresholds are tuned for, 10-bit and HDR sources included.
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
                "format=yuv420p,entropy,signalstats,metadata=print",
                "-f",
                "null",
                "-",
            ],
            options,
        )
        .await?;
        Ok(parse_keyframe_visuals(&String::from_utf8_lossy(
            &out.stderr,
        )))
    }
}

/// The `blackframe` filter's cutoff as `blackdetect`'s `pix_th`: a fraction
/// of the limited (16–235) luma range, so both filters agree on a black pixel
/// (`FormatBlackDetectPixelThreshold`).
fn pixel_threshold(threshold: i32) -> String {
    invariant_4((f64::from(threshold) - 16.0) / 219.0)
}

/// The black-frame minimum percentage as `blackdetect`'s `pic_th`
/// (`FormatBlackDetectPictureRatioThreshold`).
fn picture_threshold(minimum: i32) -> String {
    invariant_4(f64::from(minimum) / 100.0)
}

/// .NET's `ToString("0.####")` of a value clamped to `[0, 1]`.
fn invariant_4(value: f64) -> String {
    let text = format!("{:.4}", value.clamp(0.0, 1.0));
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

/// `FFmpegOutputParser`'s expressions for `blackdetect` and the keyframe
/// visuals, verbatim.
static BLACK_INTERVAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"black_start:(?<start>[-+]?(?:\d+(?:\.\d*)?|\.\d+))\s+black_end:(?<end>[-+]?(?:\d+(?:\.\d*)?|\.\d+))\s+black_duration:(?<duration>[-+]?(?:\d+(?:\.\d*)?|\.\d+))")
        .expect("black interval regex")
});
static VISUAL_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"pts_time:(?<time>-?[0-9]+(?:\.[0-9]+)?(?:[eE][-+]?[0-9]+)?)")
        .expect("visual time regex")
});
static VISUAL_ENTROPY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"lavfi\.entropy\.normalized_entropy\.normal\.Y=(?<value>-?[0-9]+(?:\.[0-9]+)?(?:[eE][-+]?[0-9]+)?)")
        .expect("visual entropy regex")
});
static VISUAL_SATURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"lavfi\.signalstats\.SATAVG=(?<value>-?[0-9]+(?:\.[0-9]+)?(?:[eE][-+]?[0-9]+)?)")
        .expect("visual saturation regex")
});

/// `ParseBlackIntervals`: complete intervals of positive length.
fn parse_black_intervals(raw: &str) -> Vec<BlackInterval> {
    raw.split('\n')
        .filter_map(|line| {
            let found = BLACK_INTERVAL.captures(line)?;
            let start: f64 = found["start"].parse().ok()?;
            let end: f64 = found["end"].parse().ok()?;
            let duration: f64 = found["duration"].parse().ok()?;
            (end > start && duration > 0.0).then_some(BlackInterval { start, end })
        })
        .collect()
}

/// `ParseKeyframeVisuals`: one visual per `pts_time` block that reported
/// both the luma entropy and the saturation.
fn parse_keyframe_visuals(raw: &str) -> Vec<KeyframeVisual> {
    let mut visuals = Vec::new();
    let mut current: Option<(f64, Option<f64>, Option<f64>)> = None;
    let mut flush = |current: Option<(f64, Option<f64>, Option<f64>)>| {
        if let Some((time, Some(entropy), Some(saturation))) = current {
            visuals.push(KeyframeVisual {
                time,
                entropy,
                saturation,
            });
        }
    };
    for line in raw.split('\n') {
        if let Some(found) = VISUAL_TIME.captures(line) {
            flush(current.take());
            current = found["time"].parse().ok().map(|time| (time, None, None));
            continue;
        }
        if let Some(found) = VISUAL_ENTROPY.captures(line) {
            if let Some(block) = current.as_mut() {
                block.1 = found["value"].parse().ok();
            }
            continue;
        }
        if let Some(found) = VISUAL_SATURATION.captures(line)
            && let Some(block) = current.as_mut()
        {
            block.2 = found["value"].parse().ok();
        }
    }
    flush(current);
    visuals
}

/// `FFmpegOutputParser`'s black-frame expression, verbatim.
static BLACK_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[Parsed_blackframe_0 @ [^\]]+\] frame:(\d+) pblack:(\d+) .*? t:([\d.]+)")
        .expect("black frame regex")
});

/// `ParseBlackFrames`: one frame per matching line. A number that does not
/// parse skips its line (upstream's `int.Parse` would throw the result away).
pub(crate) fn parse_black_frames(raw: &str) -> Vec<BlackFrame> {
    raw.split('\n')
        .filter_map(|line| {
            let found = BLACK_FRAME.captures(line)?;
            Some(BlackFrame {
                frame: found[1].parse().ok()?,
                percentage: found[2].parse().ok()?,
                time: found[3].parse().ok()?,
            })
        })
        .collect()
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
    /// Frames are wholly black from this time on (a black credits card),
    /// one every tenth of a second.
    pub black_from: Option<f64>,
    /// How black those frames are (100 when unset).
    pub black_percentage: Option<i32>,
    /// Every range scan's frames instead, as given (upstream's fake probe
    /// frames).
    pub range_frames: Option<Vec<BlackFrame>>,
    /// The keyframe black-frame scan's frames.
    pub keyframe_frames: Vec<BlackFrame>,
    /// The interval scan's intervals (absolute).
    pub intervals: Vec<BlackInterval>,
    /// The interval scan fails (`blackdetect` unavailable).
    pub fail_intervals: bool,
    /// The keyframe visual scan's visuals.
    pub visuals: Vec<KeyframeVisual>,
    /// The keyframe black-frame scan fails.
    pub fail_keyframe_scan: bool,
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

    async fn detect_black_frames(
        &self,
        path: &str,
        start: f64,
        end: f64,
        threshold: i32,
        options: ProcessOptions,
    ) -> Result<Vec<BlackFrame>, String> {
        self.record(("blackframe", path, start, end, threshold, options));
        if let Some(frames) = &self.range_frames {
            return Ok(frames.clone());
        }
        let Some(from) = self.black_from else {
            return Ok(Vec::new());
        };
        // Tenths of a second, in whole numbers to keep the times exact.
        #[allow(clippy::cast_possible_truncation)]
        let tenth = |t: f64| (t * 10.0).round() as i64;
        #[allow(clippy::cast_precision_loss)]
        Ok((tenth(start.max(from))..tenth(end))
            .map(|n| BlackFrame {
                percentage: self.black_percentage.unwrap_or(100),
                time: n as f64 / 10.0 - start,
                frame: n,
            })
            .collect())
    }

    async fn detect_keyframe_black_frames(
        &self,
        path: &str,
        start: f64,
        threshold: i32,
        options: ProcessOptions,
    ) -> Result<Vec<BlackFrame>, String> {
        self.record(("keyframe-blackframes", path, start, 0.0, threshold, options));
        if self.fail_keyframe_scan {
            return Err("keyframe scan failed".to_owned());
        }
        Ok(self.keyframe_frames.clone())
    }

    async fn detect_black_intervals(
        &self,
        path: &str,
        (start, end): (f64, f64),
        (threshold, _minimum): (i32, i32),
        options: ProcessOptions,
    ) -> Result<Vec<BlackInterval>, String> {
        self.record(("intervals", path, start, end, threshold, options));
        if self.fail_intervals {
            return Err("blackdetect unavailable".to_owned());
        }
        Ok(self.intervals.clone())
    }

    async fn detect_keyframe_visuals(
        &self,
        path: &str,
        start: f64,
        end: f64,
        options: ProcessOptions,
    ) -> Result<Vec<KeyframeVisual>, String> {
        self.record(("visuals", path, start, end, 0, options));
        Ok(self.visuals.clone())
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

    /// .NET's `ToString("0.####")` of `blackdetect`'s thresholds; 32 is the
    /// `pix_th=0.0731` upstream's `TestNoTrailingOptionsWithBlackIntervalDetection`
    /// runs.
    #[rstest::rstest]
    #[case(32, "0.0731")]
    #[case(28, "0.0548")]
    #[case(16, "0")]
    #[case(0, "0")]
    #[case(300, "1")]
    fn blackdetect_pixel_thresholds(#[case] threshold: i32, #[case] expected: &str) {
        assert_eq!(pixel_threshold(threshold), expected);
    }

    #[test]
    fn blackdetect_picture_thresholds() {
        assert_eq!(picture_threshold(85), "0.85");
        assert_eq!(picture_threshold(100), "1");
    }

    /// `TestParseBlackIntervals_*`.
    #[test]
    fn black_intervals_follow_parse_black_intervals() {
        let raw = "[blackdetect @ 0000000000000000] black_start:3.04 black_end:9.96 black_duration:6.92\n\
                   [blackdetect @ 0000000000000000] black_start:15 black_end:20.5 black_duration:5.5\n";
        assert_eq!(
            parse_black_intervals(raw),
            [
                BlackInterval {
                    start: 3.04,
                    end: 9.96
                },
                BlackInterval {
                    start: 15.0,
                    end: 20.5
                }
            ]
        );
        let invalid = "[blackdetect @ 0000000000000000] black_start:3.04\n\
                       [blackdetect @ 0000000000000000] black_start:9 black_end:8 black_duration:1\n";
        assert!(parse_black_intervals(invalid).is_empty());
    }

    /// `TestParseKeyframeVisuals_*`.
    #[rstest::rstest]
    #[case::entropy_and_saturation(
        "[Parsed_metadata_2 @ 0x0] frame:0    pts:0       pts_time:0\n\
         [Parsed_metadata_2 @ 0x0] lavfi.entropy.normalized_entropy.normal.Y=0.531285\n\
         [Parsed_metadata_2 @ 0x0] lavfi.signalstats.SATAVG=108.199\n\
         [Parsed_metadata_2 @ 0x0] frame:1    pts:20480   pts_time:2\n\
         [Parsed_metadata_2 @ 0x0] lavfi.entropy.normalized_entropy.normal.Y=0.000000\n\
         [Parsed_metadata_2 @ 0x0] lavfi.signalstats.SATAVG=33\n",
        &[(0.0, 0.531_285, 108.199), (2.0, 0.0, 33.0)]
    )]
    #[case::luma_plane_only_and_blocks_without_entropy(
        "[Parsed_metadata_3 @ 0x0] frame:0 pts:0 pts_time:5\n\
         [Parsed_metadata_3 @ 0x0] lavfi.entropy.normalized_entropy.normal.Y=0.120000\n\
         [Parsed_metadata_3 @ 0x0] lavfi.entropy.normalized_entropy.normal.U=0.400000\n\
         [Parsed_metadata_3 @ 0x0] lavfi.entropy.normalized_entropy.normal.V=0.410000\n\
         [Parsed_metadata_3 @ 0x0] lavfi.signalstats.SATAVG=12.5\n\
         [Parsed_metadata_3 @ 0x0] frame:1 pts:1 pts_time:7\n",
        &[(5.0, 0.12, 12.5)]
    )]
    #[case::blocks_without_saturation(
        "[Parsed_metadata_2 @ 0x0] frame:0 pts:0 pts_time:5\n\
         [Parsed_metadata_2 @ 0x0] lavfi.entropy.normalized_entropy.normal.Y=0.120000\n\
         [Parsed_metadata_2 @ 0x0] lavfi.signalstats.SATAVG=12.5\n\
         [Parsed_metadata_2 @ 0x0] frame:1 pts:1 pts_time:7\n\
         [Parsed_metadata_2 @ 0x0] lavfi.entropy.normalized_entropy.normal.Y=0.050000\n",
        &[(5.0, 0.12, 12.5)]
    )]
    #[case::exponent_notation(
        "[Parsed_metadata_2 @ 0x0] frame:0 pts:0 pts_time:1e-05\n\
         [Parsed_metadata_2 @ 0x0] lavfi.entropy.normalized_entropy.normal.Y=1.5e-06\n\
         [Parsed_metadata_2 @ 0x0] lavfi.signalstats.SATAVG=3.2e+01\n\
         [Parsed_metadata_2 @ 0x0] frame:1 pts:1 pts_time:2\n",
        &[(1e-05, 1.5e-06, 32.0)]
    )]
    fn keyframe_visuals_follow_parse_keyframe_visuals(
        #[case] raw: &str,
        #[case] expected: &[(f64, f64, f64)],
    ) {
        let parsed: Vec<(f64, f64, f64)> = parse_keyframe_visuals(raw)
            .iter()
            .map(|v| (v.time, v.entropy, v.saturation))
            .collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn black_frames_follow_parse_black_frames() {
        let raw = "[Parsed_blackframe_0 @ 0x0000000] frame:1 pblack:99 pts:43 t:0.043000 type:B last_keyframe:0\n\
                   [Parsed_blackframe_0 @ 0x0000000] frame:2 pblack:85 pts:85 t:0.085000 type:B last_keyframe:0\n\
                   [Parsed_showinfo_0 @ 0x0] frame:3 pblack:99 t:1\n";
        assert_eq!(
            parse_black_frames(raw),
            [
                BlackFrame {
                    percentage: 99,
                    time: 0.043,
                    frame: 1
                },
                BlackFrame {
                    percentage: 85,
                    time: 0.085,
                    frame: 2
                },
            ]
        );
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
        // Absolute times, within the window (`-to` is its duration).
        assert!(
            matches!(silences[..], [(s, e)] if (s - 3.0).abs() < 0.2 && (e - 5.0).abs() < 0.2),
            "{silences:?}"
        );
        let keyframes = service
            .detect_keyframes(clip, 2.0, 6.0, options)
            .await
            .expect("keyframes");
        let (first, last) = (keyframes[0], keyframes[keyframes.len() - 1]);
        assert!((first - 2.0).abs() < 0.05 && last <= 6.05, "{keyframes:?}");
        let audio = service
            .probe_audio_duration(clip, options)
            .await
            .expect("duration");
        assert!((audio - 8.0).abs() < 0.2, "{audio}");
        // A test pattern has no black frame; a black card is all black, with
        // times relative to the window.
        let none = service
            .detect_black_frames(clip, 0.0, 2.0, 32, options)
            .await
            .expect("black frames");
        assert!(none.is_empty(), "{none:?}");
        let black = dir.path().join("black.mkv");
        let black = black.to_str().expect("utf-8");
        let made = output(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=10:d=4",
                "-c:v",
                "libx264",
                black,
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
        let frames = service
            .detect_black_frames(black, 1.0, 3.0, 32, options)
            .await
            .expect("black frames");
        assert!(frames.len() >= 15, "{frames:?}");
        assert!(
            frames.iter().all(|f| f.percentage == 100 && f.time < 2.05),
            "{frames:?}"
        );
    }

    /// The alternative credits analyzer's scans against generated clips: a
    /// test pattern and a black card (keyframe every second).
    #[tokio::test]
    async fn real_ffmpeg_credit_scans() {
        if std::env::var_os("FERROFIN_FFMPEG_TESTS").is_none() {
            eprintln!("skipped: set FERROFIN_FFMPEG_TESTS=1 to run");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let make = |name: &str, source: &str| {
            let path = dir.path().join(name).to_string_lossy().into_owned();
            let source = source.to_owned();
            async move {
                let made = output(
                    "ffmpeg",
                    &[
                        "-v", "error", "-f", "lavfi", "-i", &source, "-c:v", "libx264", "-g", "10",
                        &path,
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
                path
            }
        };
        let clip = make("clip.mkv", "testsrc=size=64x64:rate=10:duration=4").await;
        let black = make("black.mkv", "color=c=black:s=64x64:r=10:d=4").await;
        let (clip, black) = (clip.as_str(), black.as_str());
        let service = Ffmpeg::new("ffmpeg", "ffprobe");
        let options = ProcessOptions::new("Idle", 1);
        // The credit scans: every keyframe of the card is black, and
        // `blackdetect` sees one interval (absolute times).
        let keyframes = service
            .detect_keyframe_black_frames(black, 0.0, 32, options)
            .await
            .expect("keyframe black frames");
        assert!(
            !keyframes.is_empty() && keyframes.iter().all(|f| f.percentage == 100),
            "{keyframes:?}"
        );
        let intervals = service
            .detect_black_intervals(black, (1.0, 3.0), (32, 85), options)
            .await
            .expect("intervals");
        assert!(
            matches!(intervals[..], [i] if (i.start - 1.0).abs() < 0.15 && (i.end - 3.0).abs() < 0.15),
            "{intervals:?}"
        );
        // A test pattern is busy; a black card is uniform and unsaturated.
        let busy = service
            .detect_keyframe_visuals(clip, 0.0, 4.0, options)
            .await
            .expect("visuals");
        let card = service
            .detect_keyframe_visuals(black, 0.0, 4.0, options)
            .await
            .expect("visuals");
        assert!(
            !busy.is_empty() && busy.iter().all(|v| v.entropy > 0.35),
            "{busy:?}"
        );
        assert!(
            !card.is_empty() && card.iter().all(|v| v.entropy < 0.35 && v.saturation < 96.0),
            "{card:?}"
        );
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
