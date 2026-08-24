//! The DVR — port of `Jellyfin.LiveTv.Timers.TimerManager`,
//! `Jellyfin.LiveTv.Recordings.RecordingsManager` and the two recorders.
//!
//! A timer is a channel plus a time window. When its start (minus its
//! pre-padding) arrives, the timer *fires*: the DVR opens the channel's live
//! stream through [`crate::stream`] and copies it to a file under the recordings
//! directory until the programme's end (plus its post-padding), or until the
//! timer is cancelled. While that runs the recording is *active*, and
//! `GET /LiveTv/LiveRecordings/{timerId}/stream` serves the growing file.
//!
//! Two recorders exist, chosen exactly as `RecordingsManager.GetRecorder` does:
//! [`RecorderKind::Direct`] copies the bytes verbatim (the MPEG-TS case, which
//! is what an M3U tuner gives), and [`RecorderKind::Encoded`] runs them through
//! ffmpeg with the video and audio streams copied — the remux upstream falls
//! back to when the container is not a transport stream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::live_tv::{LiveTvOptions, TimerInfoDto};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

use crate::error::LiveTvError;
use crate::stream::TunerStreamSource;

/// How much of a still-growing source one recorder read pulls, in bytes.
///
/// Port of `IODefaults.CopyToBufferSize`.
const COPY_BUFFER_BYTES: usize = 81_920;

/// How long the recorder waits before looking for more bytes, in milliseconds.
///
/// Port of `ProgressiveFileStream`'s `Task.Delay(50)` — the recorder reads the
/// live-stream buffer through the same tail-follow as an HTTP consumer.
const COPY_POLL_MS: u64 = 50;

/// How long the recorder tolerates a source that stops producing before it
/// gives up on the recording, in milliseconds.
///
/// Port of `ProgressiveFileStream`'s `timeoutMs = 30000`: a tuner that has gone
/// quiet for this long is not coming back within this programme.
const COPY_IDLE_TIMEOUT_MS: u64 = 30_000;

/// How long after a failed recording the timer is retried, in seconds.
///
/// Port of `RecordingsManager.RecordStream`'s `RetryIntervalSeconds`.
pub const RETRY_INTERVAL_SECONDS: i64 = 60;

/// How many times one timer is retried before it is given up on.
///
/// Port of `RecordingsManager.RecordStream`'s `timer.RetryCount < 10`.
pub const MAX_RETRY_COUNT: u32 = 10;

/// The `-analyzeduration` the encoded recorder gives ffmpeg, in microseconds
/// (C# `EncodedRecorder.GetCommandLineArgs`'s `analyzeDurationSeconds = 5`).
const ENCODED_ANALYZE_DURATION_US: u64 = 5_000_000;

/// The recording facts a fired timer carries.
///
/// Port of the `MediaBrowser.Controller.LiveTv.TimerInfo` fields the recorder,
/// the path builder and the recording DTO read. Ferrofin persists a timer as
/// its wire [`TimerInfoDto`] (so a `GET` round-trips exactly what was posted)
/// and resolves these programme facts from the guide row when the timer fires —
/// which is what `DefaultLiveTvService.OnTimerManagerTimerFired` does too, via
/// `CopyProgramInfoToTimerInfo` on the cached programme.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // one field per upstream TimerInfo flag
pub struct TimerRecordingInfo {
    /// The timer's id — also the key `/LiveTv/LiveRecordings/{id}/stream` takes.
    pub id: String,
    /// The programme's name, which is also the recording's.
    pub name: String,
    /// The channel being recorded.
    pub channel_id: Uuid,
    /// The programme's start.
    pub start_date: DateTime<Utc>,
    /// The programme's end.
    pub end_date: DateTime<Utc>,
    /// Seconds of recording before [`Self::start_date`].
    pub pre_padding_seconds: i32,
    /// Seconds of recording after [`Self::end_date`].
    pub post_padding_seconds: i32,
    /// The programme description.
    pub overview: Option<String>,
    /// The episode's own title, when the programme is an episode.
    pub episode_title: Option<String>,
    /// The season number, when known.
    pub season_number: Option<i32>,
    /// The episode number, when known.
    pub episode_number: Option<i32>,
    /// The production year, when known.
    pub production_year: Option<i32>,
    /// The original air date, when known.
    pub original_air_date: Option<DateTime<Utc>>,
    /// Whether the programme is an episode of a series (C#
    /// `TimerInfo.IsProgramSeries`), which selects the `Series/` layout.
    pub is_program_series: bool,
    /// Whether the programme is a movie.
    pub is_movie: bool,
    /// Whether the programme is for kids.
    pub is_kids: bool,
    /// Whether the programme is sport.
    pub is_sports: bool,
    /// Whether the programme is news.
    pub is_news: bool,
    /// Whether the programme is live.
    pub is_live: bool,
    /// Whether the airing is a repeat.
    pub is_repeat: bool,
    /// Whether the airing is a premiere.
    pub is_premiere: bool,
    /// The programme id the timer was created for (the guide item's id).
    pub program_id: Option<String>,
    /// The listing provider's own programme id.
    pub external_program_id: Option<String>,
    /// The series timer that scheduled this one, if any.
    pub series_timer_id: Option<String>,
}

impl TimerRecordingInfo {
    /// The facts a timer carries on its own, before the guide is consulted.
    #[must_use]
    pub fn from_timer(timer: &TimerInfoDto) -> Self {
        Self {
            id: timer.base.id.clone().unwrap_or_default(),
            name: timer.base.name.clone().unwrap_or_default(),
            channel_id: timer.base.channel_id,
            start_date: timer.base.start_date,
            end_date: timer.base.end_date,
            pre_padding_seconds: timer.base.pre_padding_seconds,
            post_padding_seconds: timer.base.post_padding_seconds,
            overview: timer.base.overview.clone(),
            program_id: timer.base.program_id.clone(),
            external_program_id: timer.base.external_program_id.clone(),
            series_timer_id: timer.series_timer_id.clone(),
            ..Self::default()
        }
    }

    /// The instant the capture stops: the programme's end plus its post-padding
    /// (C# `timer.EndDate.AddSeconds(timer.PostPaddingSeconds)`).
    #[must_use]
    pub fn recording_end_date(&self) -> DateTime<Utc> {
        self.end_date + chrono::Duration::seconds(i64::from(self.post_padding_seconds))
    }

    /// The instant the capture starts: the programme's start minus its
    /// pre-padding (C# `item.StartDate.AddSeconds(-item.PrePaddingSeconds)`).
    #[must_use]
    pub fn recording_start_date(&self) -> DateTime<Utc> {
        self.start_date - chrono::Duration::seconds(i64::from(self.pre_padding_seconds))
    }
}

/// A recording currently being captured.
///
/// Port of `ActiveRecordingInfo`: the entry `GetActiveRecordingPath` looks up
/// and `CancelRecording` cancels.
#[derive(Debug, Clone)]
pub struct ActiveRecording {
    /// The firing timer's id — the key both upstream and Ferrofin use.
    pub timer_id: String,
    /// The `FerrofinLiveTvRecordings` row this capture is filling.
    pub recording_id: Uuid,
    /// The file being written.
    pub path: PathBuf,
    /// When the capture started.
    pub started_at: DateTime<Utc>,
    /// Set to stop the capture (C# `ActiveRecordingInfo.CancellationTokenSource`).
    cancel: Arc<AtomicBool>,
}

impl ActiveRecording {
    /// Registers a new capture with a fresh cancellation flag.
    #[must_use]
    pub fn new(timer_id: String, recording_id: Uuid, path: PathBuf) -> Self {
        Self {
            timer_id,
            recording_id,
            path,
            started_at: Utc::now(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Stops the capture at its next read (C# `CancellationTokenSource.Cancel`).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// The flag the copy loop polls.
    #[must_use]
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }
}

/// Which recorder captures a media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderKind {
    /// Copy the bytes verbatim (C# `DirectRecorder`).
    Direct,
    /// Remux through ffmpeg (C# `EncodedRecorder`).
    Encoded,
}

impl RecorderKind {
    /// The recorder upstream picks for a source.
    ///
    /// Port of `RecordingsManager.GetRecorder`: anything that loops, is not a
    /// transport stream, or does not arrive over a file/HTTP path has to go
    /// through ffmpeg; everything else is copied byte for byte.
    #[must_use]
    pub fn choose(source: &MediaSourceInfo) -> Self {
        let is_ts = source
            .container
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .ends_with("ts");
        let readable = matches!(source.protocol, MediaProtocol::File | MediaProtocol::Http);
        if source.requires_looping || !is_ts || !readable {
            Self::Encoded
        } else {
            Self::Direct
        }
    }
}

/// Where a recorder reads its bytes from.
#[derive(Debug, Clone)]
pub enum RecordingInput {
    /// The live stream's buffer file, tailed as it grows — the equivalent of
    /// C#'s `IDirectStreamProvider.GetStream()`.
    Buffer {
        /// The buffer file.
        path: PathBuf,
        /// When the stream was opened, so a recorder that joins one somebody
        /// is already watching starts near the live edge instead of writing
        /// the whole backlog into the recording.
        opened_at: DateTime<Utc>,
    },
    /// The source's own URL, fetched over HTTP (C#
    /// `DirectRecorder.RecordFromMediaSource`).
    Url {
        /// The URL to fetch.
        url: String,
        /// The headers the tuner requires.
        headers: HashMap<String, String>,
    },
}

impl RecordingInput {
    /// The ffmpeg input argument for this source.
    #[must_use]
    pub fn ffmpeg_input(&self) -> String {
        match self {
            Self::Buffer { path, .. } => path.display().to_string(),
            Self::Url { url, .. } => url.clone(),
        }
    }
}

/// Copies a live source into `target` until `duration` elapses, the source goes
/// quiet, or `cancel` is set.
///
/// Port of `DirectRecorder`: the target is created fresh, then the bytes are
/// copied verbatim. The source is infinite, so the recorder owns the stopping.
///
/// # Errors
///
/// Fails when the target cannot be created or written, or when the source
/// cannot be opened.
pub async fn record_direct(
    tuner: &dyn TunerStreamSource,
    input: &RecordingInput,
    target: &Path,
    duration: Duration,
    cancel: &AtomicBool,
) -> Result<(), ServiceError> {
    create_target_dir(target).await?;
    let mut output = tokio::fs::File::create(target)
        .await
        .map_err(|e| LiveTvError::io(format!("create {}", target.display()), e))?;
    let deadline = tokio::time::Instant::now() + duration;

    match input {
        RecordingInput::Buffer { path, opened_at } => {
            copy_buffer_until(path, *opened_at, &mut output, deadline, cancel).await?;
        }
        RecordingInput::Url { url, headers } => {
            let mut body = tuner.open(url, headers).await?;
            while tokio::time::Instant::now() < deadline && !cancel.load(Ordering::SeqCst) {
                match body.next_chunk().await {
                    Ok(Some(chunk)) => {
                        if chunk.is_empty() {
                            continue;
                        }
                        output.write_all(&chunk).await.map_err(|e| {
                            LiveTvError::io(format!("write {}", target.display()), e)
                        })?;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(%error, "live tv: the recording source ended");
                        break;
                    }
                }
            }
        }
    }

    output
        .flush()
        .await
        .map_err(|e| LiveTvError::io(format!("flush {}", target.display()), e))?;
    Ok(())
}

/// Tails the live stream's buffer into `output` until the deadline, the
/// cancellation, or [`COPY_IDLE_TIMEOUT_MS`] of no growth.
async fn copy_buffer_until(
    source: &Path,
    opened_at: DateTime<Utc>,
    output: &mut tokio::fs::File,
    deadline: tokio::time::Instant,
    cancel: &AtomicBool,
) -> Result<(), ServiceError> {
    let mut input = tokio::fs::File::open(source)
        .await
        .map_err(|e| LiveTvError::io(format!("open {}", source.display()), e))?;
    // The same tail seek every other consumer of the buffer gets
    // (`LiveStream.GetStream`): a stream that has been open a while already
    // holds a backlog nobody asked to record.
    let age = Utc::now().signed_duration_since(opened_at).num_seconds();
    if age > ferrofin_traits::stubs::TAIL_SEEK_AFTER_SECONDS {
        let length = tokio::fs::metadata(source).await.map_or(0, |m| m.len());
        let tail = u64::try_from(ferrofin_traits::stubs::TAIL_SEEK_BYTES).unwrap_or(0);
        if let Err(error) = tokio::io::AsyncSeekExt::seek(
            &mut input,
            std::io::SeekFrom::Start(length.saturating_sub(tail)),
        )
        .await
        {
            tracing::warn!(source = %source.display(), %error, "live tv: seeking the recording source failed");
        }
    }
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut idle_ms = 0_u64;
    while tokio::time::Instant::now() < deadline && !cancel.load(Ordering::SeqCst) {
        let read = input
            .read(&mut buffer)
            .await
            .map_err(|e| LiveTvError::io(format!("read {}", source.display()), e))?;
        if read == 0 {
            if idle_ms >= COPY_IDLE_TIMEOUT_MS {
                tracing::warn!(source = %source.display(), "live tv: the recording source stopped growing");
                break;
            }
            tokio::time::sleep(Duration::from_millis(COPY_POLL_MS)).await;
            idle_ms += COPY_POLL_MS;
            continue;
        }
        idle_ms = 0;
        output.write_all(&buffer[..read]).await.map_err(|e| {
            LiveTvError::io(format!("write the recording of {}", source.display()), e)
        })?;
    }
    Ok(())
}

/// The ffmpeg arguments the encoded recorder runs.
///
/// Port of `EncodedRecorder.GetCommandLineArgs`: the input's own quirks
/// (`-fflags`, `-re`, stream looping) then a stream copy of video and audio into
/// the target, with metadata and subtitles dropped.
#[must_use]
pub fn encoded_recorder_args(source: &MediaSourceInfo, input: &str, target: &Path) -> Vec<String> {
    let mut args = vec!["-async".to_owned(), "1".to_owned()];

    let mut flags = String::new();
    if source.ignore_dts {
        flags.push_str("+igndts");
    }
    if source.ignore_index {
        flags.push_str("+ignidx");
    }
    if source.gen_pts_input {
        flags.push_str("+genpts");
    }
    if !flags.is_empty() {
        args.push("-fflags".to_owned());
        args.push(flags);
    }

    if source.read_at_native_framerate {
        args.push("-re".to_owned());
        // Upstream also passes `-readrate_catchup 100` here, but only when
        // `_mediaEncoder.EncoderVersion >= 8`; ffmpeg 7 and earlier reject the
        // option outright and the recording dies before it starts. The
        // `MediaEncoder` seam reports no version, so this takes upstream's
        // pre-8 behaviour, which every ffmpeg accepts.
    }

    if source.requires_looping {
        args.extend(
            [
                "-stream_loop",
                "-1",
                "-reconnect_at_eof",
                "1",
                "-reconnect_streamed",
                "1",
                "-reconnect_delay_max",
                "2",
            ]
            .map(ToOwned::to_owned),
        );
    }

    args.push("-analyzeduration".to_owned());
    args.push(ENCODED_ANALYZE_DURATION_US.to_string());
    args.push("-i".to_owned());
    args.push(input.to_owned());
    args.extend(
        [
            "-codec:v:0",
            "copy",
            "-fflags",
            "+genpts",
            "-map_metadata",
            "-1",
            // 0 lets ffmpeg pick; upstream passes the configured thread count,
            // which defaults to the same "auto".
            "-threads",
            "0",
            "-codec:a:0",
            "copy",
            "-sn",
            "-y",
        ]
        .map(ToOwned::to_owned),
    );
    args.push(target.display().to_string());
    args
}

/// Records through ffmpeg, stopping at the duration or the cancellation.
///
/// Port of `EncodedRecorder.Record`: ffmpeg is asked to quit politely (`q` on
/// stdin) and killed if it does not.
///
/// # Errors
///
/// Fails when ffmpeg cannot be spawned or exits without producing anything.
pub async fn record_encoded(
    ffmpeg_path: &str,
    source: &MediaSourceInfo,
    input: &RecordingInput,
    target: &Path,
    duration: Duration,
    cancel: &AtomicBool,
) -> Result<(), ServiceError> {
    create_target_dir(target).await?;
    let args = encoded_recorder_args(source, &input.ffmpeg_input(), target);
    let mut child = tokio::process::Command::new(ffmpeg_path)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| LiveTvError::io(format!("spawn {ffmpeg_path}"), e))?;

    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| LiveTvError::io("wait for the recording process".to_owned(), e))?
        {
            // C# `OnFfMpegProcessExited` throws on a nonzero exit, which is
            // what drives the retry — a failing ffmpeg must not look like a
            // clean capture.
            if status.success() {
                tracing::info!(?status, "live tv: the recording process exited");
                return Ok(());
            }
            return Err(ServiceError::backend(format!(
                "the recording process exited with {status}"
            )));
        }
        if tokio::time::Instant::now() >= deadline || cancel.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(COPY_POLL_MS)).await;
    }

    // Ask ffmpeg to finish the file it is writing, then insist.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q\n").await;
        let _ = stdin.flush().await;
    }
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(status)) => tracing::info!(?status, "live tv: the recording process stopped"),
        _ => {
            let _ = child.kill().await;
        }
    }
    Ok(())
}

/// Creates the directory a recording is written into.
async fn create_target_dir(target: &Path) -> Result<(), ServiceError> {
    let Some(parent) = target.parent() else {
        return Err(ServiceError::backend(format!(
            "{} has no parent directory",
            target.display()
        )));
    };
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| LiveTvError::io(format!("create {}", parent.display()), e))?;
    Ok(())
}

/// Sanitizes one path segment.
///
/// Port of `IFileSystem.GetValidFilename`: trims, then strips the characters
/// that are illegal in a path segment (separators and the Windows-reserved set).
#[must_use]
pub fn valid_filename(name: &str) -> String {
    const INVALID: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    let cleaned = name.trim().replace(INVALID, "");
    // Guide text comes from whatever M3U/XMLTV the user pointed at, so a
    // programme really can be called `..`. Upstream's `GetInvalidFileNameChars`
    // strips only the separators on Linux and would happily build
    // `{root}/../{file}`; a segment that is nothing but dots is never a name.
    if !cleaned.is_empty() && cleaned.chars().all(|c| c == '.') {
        return String::new();
    }
    cleaned
}

/// The file name a recording is written under, without its extension.
///
/// Port of `RecordingHelper.GetRecordingName`: a series episode is named by its
/// season/episode numbers, else by its air date, and gains its episode title
/// when the result stays under the 250-byte budget a filename has to leave room
/// for an extension; a movie gains its year; anything else gains the date.
#[must_use]
pub fn recording_name(timer: &TimerRecordingInfo) -> String {
    /// The byte budget a recording name may occupy before the episode title is
    /// dropped, leaving room for the extension (C# `< 250`).
    const NAME_BYTE_BUDGET: usize = 250;

    use std::fmt::Write as _;
    let mut name = timer.name.clone();
    if timer.is_program_series {
        let mut add_hyphen = true;
        match (timer.season_number, timer.episode_number) {
            (Some(season), Some(episode)) => {
                let _ = write!(name, " S{season:02}E{episode:02}");
                add_hyphen = false;
            }
            _ => {
                if let Some(original) = timer.original_air_date {
                    if original.date_naive() == timer.start_date.date_naive() {
                        name.push(' ');
                        name.push_str(&date_string(timer.start_date));
                    } else {
                        name.push(' ');
                        name.push_str(&original.format("%Y-%m-%d").to_string());
                    }
                } else {
                    name.push(' ');
                    name.push_str(&date_string(timer.start_date));
                }
            }
        }
        if let Some(episode_title) = timer
            .episode_title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let mut candidate = name.clone();
            if add_hyphen {
                candidate.push_str(" -");
            }
            candidate.push(' ');
            candidate.push_str(episode_title);
            if candidate.len() < NAME_BYTE_BUDGET {
                name = candidate;
            }
        }
    } else if timer.is_movie && timer.production_year.is_some() {
        let _ = write!(name, " ({})", timer.production_year.unwrap_or_default());
    } else {
        name.push(' ');
        name.push_str(&date_string(timer.start_date));
    }
    name
}

/// The timestamp component of a recording name (C# `GetDateString`, which
/// formats the *local* time).
fn date_string(date: DateTime<Utc>) -> String {
    chrono::DateTime::<chrono::Local>::from(date)
        .format("%Y_%m_%d_%H_%M_%S")
        .to_string()
}

/// Where a recording is written, and the series folder it belongs to (if any).
///
/// Port of `RecordingsManager.GetRecordingPath`: the kind of programme selects
/// the configured root and the optional `Series`/`Movies`/`Kids`/`Sports`/
/// `Other` subfolder, then the programme's name (and season, for a series) makes
/// the folder and [`recording_name`] the file.
#[must_use]
pub fn recording_path(
    timer: &TimerRecordingInfo,
    options: &LiveTvOptions,
    data_dir: &Path,
) -> (PathBuf, Option<PathBuf>) {
    let default_root: PathBuf = options
        .recording_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map_or_else(|| data_dir.join("livetv").join("recordings"), PathBuf::from);

    // A custom root only skips the kind subfolder when it differs from the
    // default one (C# `allowSubfolder = customRecordingPath == recordingPath`).
    let custom_root = |custom: Option<&String>| -> (PathBuf, bool) {
        match custom
            .map(String::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(path) => {
                let allow = Path::new(path) == default_root;
                (PathBuf::from(path), allow)
            }
            None => (default_root.clone(), true),
        }
    };

    let mut series_path = None;
    let mut folder = if timer.is_program_series {
        let (root, allow_subfolder) = custom_root(options.series_recording_path.as_ref());
        let mut path = root;
        if allow_subfolder && options.enable_recording_subfolders {
            path = path.join("Series");
        }
        path = path.join(folder_name(&timer.name, None));
        series_path = Some(path.clone());
        if let Some(season) = timer.season_number {
            path = path.join(format!("Season {season}"));
        }
        path
    } else if timer.is_movie {
        let (root, allow_subfolder) = custom_root(options.movie_recording_path.as_ref());
        let mut path = root;
        if allow_subfolder && options.enable_recording_subfolders {
            path = path.join("Movies");
        }
        path.join(folder_name(&timer.name, timer.production_year))
    } else if timer.is_kids {
        let mut path = default_root.clone();
        if options.enable_recording_subfolders {
            path = path.join("Kids");
        }
        path.join(folder_name(&timer.name, timer.production_year))
    } else if timer.is_sports {
        let mut path = default_root.clone();
        if options.enable_recording_subfolders {
            path = path.join("Sports");
        }
        path.join(valid_filename(&timer.name))
    } else {
        let mut path = default_root.clone();
        if options.enable_recording_subfolders {
            path = path.join("Other");
        }
        path.join(valid_filename(&timer.name))
    };

    folder = folder.join(format!("{}.ts", valid_filename(&recording_name(timer))));
    (folder, series_path)
}

/// A recording folder's name: the sanitized programme name, optionally with its
/// year, with any trailing period trimmed (C# `.Trim().TrimEnd('.').Trim()`).
fn folder_name(name: &str, year: Option<i32>) -> String {
    use std::fmt::Write as _;
    let mut folder = valid_filename(name);
    if let Some(year) = year {
        let _ = write!(folder, " ({year})");
    }
    folder.trim_end_matches('.').trim().to_owned()
}

/// Makes `path` unique against what is on disk and what other timers are
/// already recording.
///
/// Port of `RecordingsManager.EnsureFileUnique`.
#[must_use]
pub fn ensure_file_unique(path: &Path, timer_id: &str, active: &[ActiveRecording]) -> PathBuf {
    use std::fmt::Write as _;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("ts")
        .to_owned();

    let mut candidate = path.to_path_buf();
    let mut name = stem;
    let mut index = 1;
    while candidate.exists()
        || active
            .iter()
            .any(|r| r.path == candidate && !r.timer_id.eq_ignore_ascii_case(timer_id))
    {
        let _ = write!(name, " - {index}");
        candidate = parent.join(format!("{name}.{extension}"));
        index += 1;
    }
    candidate
}

/// Whether a recording's file exists but holds nothing.
///
/// Port of `RecordingsManager.DeleteFileIfEmpty`'s predicate — a zero-byte file
/// is a failed capture, not a recording.
pub async fn is_empty_file(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok_and(|m| m.len() == 0)
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveRecording, RecorderKind, RecordingInput, TimerRecordingInfo, encoded_recorder_args,
        ensure_file_unique, record_direct, recording_name, recording_path, valid_filename,
    };
    use chrono::TimeZone as _;
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::live_tv::LiveTvOptions;
    use ferrofin_model::media_info::MediaProtocol;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn timer(name: &str) -> TimerRecordingInfo {
        TimerRecordingInfo {
            id: "t1".to_owned(),
            name: name.to_owned(),
            start_date: chrono::Utc
                .with_ymd_and_hms(2026, 8, 23, 17, 0, 0)
                .single()
                .expect("start"),
            end_date: chrono::Utc
                .with_ymd_and_hms(2026, 8, 23, 18, 0, 0)
                .single()
                .expect("end"),
            ..TimerRecordingInfo::default()
        }
    }

    #[test]
    fn a_series_recording_is_named_by_season_and_episode() {
        let mut t = timer("Parity Show");
        t.is_program_series = true;
        t.season_number = Some(2);
        t.episode_number = Some(7);
        t.episode_title = Some("The One With The Port".to_owned());
        // S{00}E{00}, then the episode title WITHOUT a hyphen (the numbers
        // already separate them) — upstream's `addHyphen = false` branch.
        assert_eq!(
            recording_name(&t),
            "Parity Show S02E07 The One With The Port"
        );
    }

    #[test]
    fn a_series_recording_without_numbers_falls_back_to_the_date_and_hyphenates() {
        let mut t = timer("Parity Show");
        t.is_program_series = true;
        t.episode_title = Some("Untitled".to_owned());
        let name = recording_name(&t);
        assert!(name.starts_with("Parity Show 2026_08_23_"), "{name}");
        assert!(name.ends_with(" - Untitled"), "{name}");
    }

    #[test]
    fn an_over_long_episode_title_is_dropped_rather_than_truncated() {
        let mut t = timer("Parity Show");
        t.is_program_series = true;
        t.season_number = Some(1);
        t.episode_number = Some(1);
        t.episode_title = Some("x".repeat(400));
        assert_eq!(recording_name(&t), "Parity Show S01E01");
    }

    #[test]
    fn a_movie_recording_is_named_by_its_year_and_anything_else_by_its_date() {
        let mut t = timer("Parity Movie");
        t.is_movie = true;
        t.production_year = Some(1999);
        assert_eq!(recording_name(&t), "Parity Movie (1999)");

        let plain = timer("Parity News");
        assert!(recording_name(&plain).starts_with("Parity News 2026_08_23_"));
    }

    #[test]
    fn the_recording_layout_follows_the_programme_kind() {
        let data = Path::new("/data");
        let options = LiveTvOptions {
            enable_recording_subfolders: true,
            ..LiveTvOptions::default()
        };
        let root = Path::new("/data/livetv/recordings");

        let mut series = timer("Parity Show");
        series.is_program_series = true;
        series.season_number = Some(3);
        let (path, series_folder) = recording_path(&series, &options, data);
        assert_eq!(
            path.parent().expect("parent"),
            root.join("Series").join("Parity Show").join("Season 3")
        );
        assert_eq!(
            series_folder.expect("series folder"),
            root.join("Series").join("Parity Show")
        );
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("ts"));

        let mut movie = timer("Parity Movie");
        movie.is_movie = true;
        movie.production_year = Some(1999);
        let (path, series_folder) = recording_path(&movie, &options, data);
        assert_eq!(
            path.parent().expect("parent"),
            root.join("Movies").join("Parity Movie (1999)")
        );
        assert!(series_folder.is_none());

        // Nothing in particular: `Other/{Name}` — the fixture programme's case.
        let (path, _) = recording_path(&timer("Parity News"), &options, data);
        assert_eq!(
            path.parent().expect("parent"),
            root.join("Other").join("Parity News")
        );

        // Subfolders off: straight under the root.
        let (path, _) = recording_path(&timer("Parity News"), &LiveTvOptions::default(), data);
        assert_eq!(path.parent().expect("parent"), root.join("Parity News"));
    }

    #[test]
    fn a_configured_recording_root_replaces_the_default_one() {
        let options = LiveTvOptions {
            recording_path: Some("/mnt/dvr".to_owned()),
            enable_recording_subfolders: true,
            ..LiveTvOptions::default()
        };
        let (path, _) = recording_path(&timer("Parity News"), &options, Path::new("/data"));
        assert_eq!(
            path.parent().expect("parent"),
            Path::new("/mnt/dvr/Other/Parity News")
        );
    }

    #[test]
    fn a_path_separator_never_escapes_the_recording_folder() {
        let (path, _) = recording_path(
            &timer("../../etc/passwd"),
            &LiveTvOptions::default(),
            Path::new("/data"),
        );
        assert!(
            path.starts_with("/data/livetv/recordings"),
            "a hostile programme name must stay inside the recordings root: {}",
            path.display()
        );
        assert_eq!(valid_filename("  a/b:c  "), "abc");

        // A name that is nothing but dots must not become a `..` segment.
        assert_eq!(valid_filename(".."), "");
        assert_eq!(valid_filename(" . "), "");
        let (path, _) = recording_path(&timer(".."), &LiveTvOptions::default(), Path::new("/data"));
        assert!(
            path.starts_with("/data/livetv/recordings")
                && !path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir),
            "{}",
            path.display()
        );
    }

    #[test]
    fn a_taken_path_gets_the_next_index() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("Show.ts");
        std::fs::write(&path, b"x").expect("write");
        assert_eq!(
            ensure_file_unique(&path, "t1", &[]),
            dir.path().join("Show - 1.ts")
        );
        // Another timer already recording to it counts too; this timer's own
        // in-flight path does not.
        let taken = ActiveRecording::new("t2".to_owned(), uuid::Uuid::nil(), path.clone());
        let free = dir.path().join("Other.ts");
        assert_eq!(
            ensure_file_unique(
                &free,
                "t1",
                &[ActiveRecording::new(
                    "t2".to_owned(),
                    uuid::Uuid::nil(),
                    free.clone()
                )]
            ),
            dir.path().join("Other - 1.ts")
        );
        assert_eq!(
            ensure_file_unique(&free, "t2", &[taken]),
            free,
            "the timer's own path is not a conflict"
        );
    }

    #[test]
    fn the_recorder_choice_follows_the_container_and_protocol() {
        let ts = MediaSourceInfo {
            container: Some("ts".to_owned()),
            protocol: MediaProtocol::Http,
            ..MediaSourceInfo::default()
        };
        assert_eq!(RecorderKind::choose(&ts), RecorderKind::Direct);

        let looping = MediaSourceInfo {
            requires_looping: true,
            ..ts.clone()
        };
        assert_eq!(RecorderKind::choose(&looping), RecorderKind::Encoded);

        let mkv = MediaSourceInfo {
            container: Some("mkv".to_owned()),
            ..ts.clone()
        };
        assert_eq!(RecorderKind::choose(&mkv), RecorderKind::Encoded);

        let rtsp = MediaSourceInfo {
            protocol: MediaProtocol::Rtsp,
            ..ts
        };
        assert_eq!(RecorderKind::choose(&rtsp), RecorderKind::Encoded);
    }

    #[test]
    fn the_encoded_recorder_copies_both_streams_and_drops_subtitles() {
        let source = MediaSourceInfo {
            ignore_dts: true,
            ignore_index: true,
            read_at_native_framerate: true,
            ..MediaSourceInfo::default()
        };
        let args = encoded_recorder_args(&source, "/tmp/in.ts", Path::new("/rec/out.ts"));
        let line = args.join(" ");
        assert!(line.contains("-async 1"), "{line}");
        assert!(line.contains("-fflags +igndts+ignidx"), "{line}");
        assert!(line.contains("-re"), "{line}");
        // `-readrate_catchup` is an ffmpeg 8+ option upstream gates on the
        // encoder version; ffmpeg 7 rejects it outright.
        assert!(!line.contains("-readrate_catchup"), "{line}");
        assert!(line.contains("-analyzeduration 5000000"), "{line}");
        assert!(line.contains("-i /tmp/in.ts"), "{line}");
        assert!(line.contains("-codec:v:0 copy"), "{line}");
        assert!(line.contains("-codec:a:0 copy"), "{line}");
        assert!(line.contains("-sn"), "{line}");
        assert!(line.ends_with("/rec/out.ts"), "{line}");
    }

    #[tokio::test]
    async fn a_direct_recording_copies_the_buffer_until_it_is_cancelled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let buffer = dir.path().join("live.ts");
        tokio::fs::write(&buffer, vec![0x47_u8; 4096])
            .await
            .expect("buffer");
        let target = dir.path().join("Other").join("Show.ts");

        let cancel = Arc::new(AtomicBool::new(false));
        let stopper = Arc::clone(&cancel);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            stopper.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let tuner = crate::stream::tests::LoopingTuner {
            chunk: Vec::new(),
            opens: Arc::new(AtomicUsize::new(0)),
            content_type: None,
        };
        record_direct(
            &tuner,
            &RecordingInput::Buffer {
                path: buffer,
                opened_at: chrono::Utc::now(),
            },
            &target,
            std::time::Duration::from_secs(30),
            &cancel,
        )
        .await
        .expect("record");

        // The directory was created and the buffered bytes landed verbatim.
        let recorded = tokio::fs::read(&target).await.expect("recording");
        assert_eq!(recorded.len(), 4096);
        assert!(recorded.iter().all(|b| *b == 0x47));
    }

    #[tokio::test]
    async fn a_direct_recording_from_a_url_stops_at_its_duration() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("Url.ts");
        let tuner = crate::stream::tests::LoopingTuner {
            chunk: vec![0x47_u8; 188],
            opens: Arc::new(AtomicUsize::new(0)),
            content_type: None,
        };
        record_direct(
            &tuner,
            &RecordingInput::Url {
                url: "http://tuner/live".to_owned(),
                headers: std::collections::HashMap::new(),
            },
            &target,
            std::time::Duration::from_millis(80),
            &AtomicBool::new(false),
        )
        .await
        .expect("record");
        let size = tokio::fs::metadata(&target).await.expect("recording").len();
        assert!(size > 0, "the endless tuner must have produced something");
    }

    #[test]
    fn an_ffmpeg_input_names_the_buffer_or_the_url() {
        assert_eq!(
            RecordingInput::Buffer {
                path: PathBuf::from("/x/y.ts"),
                opened_at: chrono::Utc::now(),
            }
            .ffmpeg_input(),
            "/x/y.ts"
        );
        assert_eq!(
            RecordingInput::Url {
                url: "http://t/live".to_owned(),
                headers: std::collections::HashMap::new(),
            }
            .ffmpeg_input(),
            "http://t/live"
        );
    }
}
