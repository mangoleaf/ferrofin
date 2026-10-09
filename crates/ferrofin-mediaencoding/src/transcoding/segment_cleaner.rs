//! Rolling HLS cleanup from Jellyfin's `TranscodingSegmentCleaner`.
//!
//! Retention follows finalized segment responses, independently of the final
//! job teardown. A job owns its timer, which reads live encoding settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::media_encoding::TranscodingJobType;

use crate::encoder::FfmpegVersion;
use crate::encoding_helper::EncodingJobInfo;
use crate::encoding_helper::hw::versions::{
    MIN_FFMPEG_READRATE_CATCHUP_OPTION, MIN_FFMPEG_READRATE_OPTION,
};

use super::segment_transcoder::TranscodeChild;
use super::throttler::effective_input_protocol;

const CLEAN_INTERVAL: Duration = Duration::from_secs(20);
const DELETE_DELAY: Duration = Duration::from_millis(1500);
const TICKS_PER_SECOND: f64 = 10_000_000.0;
const MIN_RUNTIME_TICKS: i64 = 300 * 10_000_000;

/// Input pacing required by `EncodingHelper.GetInputModifier`.
///
/// Native-frame-rate sources use `-re`, except RTSP. With rolling deletion
/// enabled, copied video HLS instead reads at 10 times playback speed, keeping
/// ffmpeg alive long enough for retention to follow the consumer. FFmpeg 8 adds
/// the pinned upstream catch-up option; older versions must not receive it.
#[must_use]
pub fn input_rate_arguments(
    state: &EncodingJobInfo,
    options: &EncodingOptions,
    version: Option<FfmpegVersion>,
) -> Vec<String> {
    let mut arguments = Vec::new();
    let rate = if state.media_source.read_at_native_framerate
        && effective_input_protocol(state) != MediaProtocol::Rtsp
    {
        arguments.push("-re".to_owned());
        1
    } else if options.enable_segment_deletion
        && state.video_stream.is_some()
        && state.transcoding_type == TranscodingJobType::Hls
        && EncodingJobInfo::is_copy_codec(state.output_video_codec.as_deref())
        && version.is_some_and(|version| version >= MIN_FFMPEG_READRATE_OPTION)
    {
        arguments.extend(["-readrate".to_owned(), "10".to_owned()]);
        10
    } else {
        0
    };
    if rate > 0 && version.is_some_and(|version| version >= MIN_FFMPEG_READRATE_CATCHUP_OPTION) {
        arguments.extend(["-readrate_catchup".to_owned(), (rate * 100).to_string()]);
    }
    arguments
}

pub(crate) fn eligible(state: &EncodingJobInfo) -> bool {
    matches!(
        effective_input_protocol(state),
        MediaProtocol::File | MediaProtocol::Http
    ) && state.is_input_video
        && state.transcoding_type == TranscodingJobType::Hls
        && state
            .run_time_ticks
            .is_some_and(|runtime| runtime >= MIN_RUNTIME_TICKS)
}

fn maximum_deleted_index(
    options: &EncodingOptions,
    download_position_ticks: Option<i64>,
    segment_length_secs: i32,
) -> Option<i64> {
    if !options.enable_segment_deletion || segment_length_secs <= 0 {
        return None;
    }
    let keep_seconds = i64::from(options.segment_keep_seconds.max(20));
    // Convert.ToInt64(TimeSpan.TotalSeconds) rounds midpoint ties to even.
    // An i64 tick count divided by 10,000,000 is safely within i64 seconds.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let download_seconds =
        (download_position_ticks.unwrap_or(0) as f64 / TICKS_PER_SECOND).round_ties_even() as i64;
    if download_seconds <= keep_seconds {
        return None;
    }
    let index = (download_seconds - keep_seconds) / i64::from(segment_length_secs);
    (index > 0).then_some(index)
}

fn segment_index(path: &Path, playlist_stem: &str) -> Option<i64> {
    // The source removes ALL ordinal occurrences of the playlist stem, then
    // parses the basename as Int64. It does not filter by segment extension.
    path.file_stem()?
        .to_str()?
        .replace(playlist_stem, "")
        .trim_matches(|character| matches!(character, '\u{0009}'..='\u{000d}' | ' '))
        .parse()
        .ok()
}

#[cfg(test)]
fn delete_segments(playlist: &Path, maximum: i64) -> std::io::Result<()> {
    delete_segments_with(
        playlist,
        maximum,
        &|path| std::fs::remove_file(path),
        &|| false,
    )
}

fn delete_segments_with(
    playlist: &Path,
    maximum: i64,
    remove_file: &dyn Fn(&Path) -> std::io::Result<()>,
    stopped: &dyn Fn() -> bool,
) -> std::io::Result<()> {
    let directory = playlist.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "playlist has no parent")
    })?;
    let stem = playlist
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "playlist has no stem")
        })?;
    let mut first_error = None;
    for entry in std::fs::read_dir(directory)? {
        if stopped() {
            break;
        }
        let entry = entry?;
        let path = entry.path();
        if !segment_index(&path, stem).is_some_and(|index| index >= 0 && index <= maximum) {
            continue;
        }
        // GetFilePaths selects files; a directory with a numeric name is not
        // an HLS segment file and must not be recursively removed.
        if entry.file_type()?.is_dir() {
            continue;
        }
        tracing::debug!(path = %path.display(), "deleting expired HLS segment");
        if let Err(error) = remove_file(&path) {
            tracing::debug!(path = %path.display(), %error, "deleting HLS segment failed");
            // Like the source aggregate, one failed removal does not prevent
            // attempts to remove the other matching files.
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

type DownloadPosition = dyn Fn() -> Option<i64> + Send + Sync;

/// Dropping the owned task cancels its next tick or pending deletion delay.
/// A removal batch stops between files; an already running unlink may finish.
/// Normal process exit stops at the next tick.
pub(crate) struct SegmentCleanerTask {
    task: tokio::task::JoinHandle<()>,
    stopped: Arc<AtomicBool>,
}

impl SegmentCleanerTask {
    pub(crate) fn start(
        child: Arc<dyn TranscodeChild>,
        config: Arc<dyn ServerConfigurationManager>,
        playlist: PathBuf,
        segment_length_secs: i32,
        download_position: Arc<DownloadPosition>,
    ) -> Self {
        Self::start_with_timing(
            child,
            config,
            playlist,
            segment_length_secs,
            download_position,
            CLEAN_INTERVAL,
            DELETE_DELAY,
        )
    }

    fn start_with_timing(
        child: Arc<dyn TranscodeChild>,
        config: Arc<dyn ServerConfigurationManager>,
        playlist: PathBuf,
        segment_length_secs: i32,
        download_position: Arc<DownloadPosition>,
        interval: Duration,
        delete_delay: Duration,
    ) -> Self {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let first_tick = tokio::time::Instant::now() + interval;
        let task = tokio::spawn(async move {
            let mut timer = tokio::time::interval_at(first_tick, interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                if child.has_exited() {
                    return;
                }
                let options = match config.get_encoding_options().await {
                    Ok(options) => options,
                    Err(error) => {
                        tracing::debug!(%error, "reading HLS retention settings failed");
                        continue;
                    }
                };
                let Some(maximum) =
                    maximum_deleted_index(&options, download_position(), segment_length_secs)
                else {
                    continue;
                };
                tracing::debug!(
                    path = %playlist.display(),
                    maximum,
                    "scheduling expired HLS segment deletion"
                );
                tokio::time::sleep(delete_delay).await;
                // Directory enumeration and unlink are blocking operations.
                // Keep at most one batch per job off the HTTP executor. A
                // started blocking batch observes teardown between removals.
                let deleting_playlist = playlist.clone();
                let deleting_stopped = Arc::clone(&worker_stopped);
                let removal = tokio::task::spawn_blocking(move || {
                    if deleting_stopped.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    delete_segments_with(
                        &deleting_playlist,
                        maximum,
                        &|path| std::fs::remove_file(path),
                        &|| deleting_stopped.load(Ordering::Acquire),
                    )
                })
                .await;
                match removal {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::debug!(path = %playlist.display(), %error, "rolling HLS cleanup failed");
                    }
                    Err(error) => {
                        tracing::debug!(path = %playlist.display(), %error, "rolling HLS cleanup worker failed");
                    }
                }
            }
        });
        Self { task, stopped }
    }
}

impl Drop for SegmentCleanerTask {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use crate::transcoding::segment_transcoder::{
        FakeScript, FakeSegmentTranscoder, SegmentTranscoder, SpawnRequest,
    };

    fn state() -> EncodingJobInfo {
        EncodingJobInfo {
            display: crate::encoding_helper::TranscodeDisplayNames::default(),
            base_request: crate::encoding_helper::BaseEncodingJobOptions::default(),
            video_stream: Some(ferrofin_model::entities_media::MediaStream::default()),
            audio_stream: None,
            subtitle_stream: None,
            media_source: ferrofin_model::dto::MediaSourceInfo::default(),
            output_video_codec: Some("copy".to_owned()),
            output_audio_codec: None,
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: None,
            output_video_sync: None,
            output_file_path: String::new(),
            input_container: None,
            is_input_video: true,
            subtitle_delivery_method: ferrofin_model::dlna::SubtitleDeliveryMethod::Encode,
            run_time_ticks: Some(MIN_RUNTIME_TICKS),
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            segment_length_secs: 6,
            wait_for_path: None,
            segment_container: Some("ts".to_owned()),
            play_session_id: None,
            device_id: None,
        }
    }

    #[test]
    fn cleaner_only_enrolls_the_pinned_eligible_job_families() {
        for protocol in [
            MediaProtocol::File,
            MediaProtocol::Http,
            MediaProtocol::Rtsp,
            MediaProtocol::Ftp,
        ] {
            for kind in [
                TranscodingJobType::Hls,
                TranscodingJobType::Dash,
                TranscodingJobType::Progressive,
            ] {
                for runtime in [None, Some(MIN_RUNTIME_TICKS - 1), Some(MIN_RUNTIME_TICKS)] {
                    for video in [false, true] {
                        let mut state = state();
                        state.media_source.protocol = protocol;
                        state.transcoding_type = kind;
                        state.run_time_ticks = runtime;
                        state.is_input_video = video;
                        assert_eq!(
                            eligible(&state),
                            matches!(protocol, MediaProtocol::File | MediaProtocol::Http)
                                && kind == TranscodingJobType::Hls
                                && runtime == Some(MIN_RUNTIME_TICKS)
                                && video
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cleaner_uses_input_video_identity_even_for_an_audio_only_output() {
        let mut state = state();
        state.video_stream = None;
        state.audio_stream = Some(ferrofin_model::entities_media::MediaStream::default());
        state.output_video_codec = None;
        assert!(
            eligible(&state),
            "input MediaType=Video remains eligible after audio-only output selection"
        );
        let options = EncodingOptions {
            enable_segment_deletion: true,
            ..EncodingOptions::default()
        };
        assert!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))).is_empty(),
            "copy pacing independently requires a selected video stream"
        );
        state.is_input_video = false;
        assert!(!eligible(&state), "an actual audio input does not enroll");
    }

    #[test]
    fn encoder_protocol_only_overrides_when_an_encoder_path_exists() {
        let mut state = state();
        state.media_source.protocol = MediaProtocol::Rtsp;
        state.media_source.encoder_protocol = Some(MediaProtocol::File);
        assert!(!eligible(&state));
        state.media_source.encoder_path = Some(String::new());
        assert!(!eligible(&state));
        state.media_source.encoder_path = Some("/owned/buffer.ts".to_owned());
        assert!(eligible(&state));
        state.media_source.encoder_protocol = None;
        assert!(!eligible(&state));
        state.media_source.protocol = MediaProtocol::File;
        state.media_source.encoder_protocol = Some(MediaProtocol::Rtsp);
        assert!(!eligible(&state));
    }

    #[test]
    fn deletion_uses_finalized_download_retention_floor_and_inclusive_boundary() {
        let mut options = EncodingOptions {
            enable_segment_deletion: true,
            segment_keep_seconds: 0,
            ..EncodingOptions::default()
        };
        for (ticks, expected) in [
            (None, None),
            (Some(-1), None),
            (Some(200_000_000), None),
            (Some(250_000_000), None),
            (Some(260_000_000), Some(1)),
            (Some(320_000_000), Some(2)),
        ] {
            assert_eq!(maximum_deleted_index(&options, ticks, 6), expected);
        }
        for keep in [i32::MIN, -1, 0, 19, 20] {
            options.segment_keep_seconds = keep;
            assert_eq!(
                maximum_deleted_index(&options, Some(260_000_000), 6),
                Some(1)
            );
        }
        options.segment_keep_seconds = i32::MAX;
        assert_eq!(maximum_deleted_index(&options, Some(260_000_000), 6), None);
        options.segment_keep_seconds = 0;
        assert_eq!(maximum_deleted_index(&options, Some(i64::MIN), 6), None);
        // The midpoint is rounded to the even second: 25.5 ->26,26.5 ->26,
        // 31.5 ->32 and32.5 ->32. Truncation would delete a different range.
        for (ticks, expected) in [
            (255_000_000, 1),
            (265_000_000, 1),
            (315_000_000, 2),
            (325_000_000, 2),
        ] {
            assert_eq!(
                maximum_deleted_index(&options, Some(ticks), 6),
                Some(expected)
            );
        }
        options.segment_keep_seconds = 30;
        assert_eq!(maximum_deleted_index(&options, Some(320_000_000), 6), None);
        assert_eq!(
            maximum_deleted_index(&options, Some(360_000_000), 6),
            Some(1)
        );
        options.enable_segment_deletion = false;
        assert_eq!(
            maximum_deleted_index(&options, Some(3_600_000_000), 6),
            None
        );
        options.enable_segment_deletion = true;
        assert_eq!(maximum_deleted_index(&options, Some(i64::MAX), 0), None);
        assert!(maximum_deleted_index(&options, Some(i64::MAX), 1).is_some());
    }

    #[test]
    fn deletion_preserves_init_playlist_future_partial_and_other_job_files() {
        let dir = tempfile::tempdir().unwrap();
        let deleted = [
            "out0.ts",
            "out1.mp4",
            "out2.ts",
            "outout2.txt",
            "out+1.ts",
            "out 1 .ts",
            "0.ts",
        ];
        let kept = [
            "out-1.mp4",
            "out3.ts",
            "out2.ts.tmp",
            "out.m3u8",
            "out.log",
            "OTHER0.ts",
            "OUT0.ts",
            "out9223372036854775808.ts",
            "out\u{00a0}1.ts",
        ];
        for name in deleted.iter().chain(&kept) {
            std::fs::write(dir.path().join(name), b"segment").unwrap();
        }
        std::fs::create_dir(dir.path().join("out1.dir")).unwrap();
        delete_segments(&dir.path().join("out.m3u8"), 2).unwrap();
        for name in deleted {
            assert!(!dir.path().join(name).exists(), "{name}");
        }
        for name in kept {
            assert!(dir.path().join(name).exists(), "{name}");
        }
        assert!(dir.path().join("out1.dir").is_dir());
        assert!(delete_segments(&dir.path().join("missing/out.m3u8"), 2).is_err());
        assert!(delete_segments(Path::new("/"), 2).is_err());
        assert!(delete_segments(Path::new("."), 2).is_err());
    }

    #[test]
    fn a_failed_removal_does_not_skip_other_matching_segments() {
        let dir = tempfile::tempdir().unwrap();
        let failed = dir.path().join("out0.ts");
        let removed = dir.path().join("out1.ts");
        std::fs::write(&failed, b"segment").unwrap();
        std::fs::write(&removed, b"segment").unwrap();
        let attempted = Mutex::new(Vec::new());
        let result = delete_segments_with(
            &dir.path().join("out.m3u8"),
            1,
            &|path| {
                attempted.lock().unwrap().push(path.to_owned());
                if path == failed {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "controlled removal failure",
                    ))
                } else {
                    std::fs::remove_file(path)
                }
            },
            &|| false,
        );
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(failed.exists());
        assert!(!removed.exists());
        let attempted = attempted.lock().unwrap();
        assert_eq!(attempted.len(), 2);
        assert!(attempted.contains(&failed));
        assert!(attempted.contains(&removed));
    }

    #[test]
    fn teardown_stops_a_removal_batch_between_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["out0.ts", "out1.ts"] {
            std::fs::write(dir.path().join(name), b"segment").unwrap();
        }
        let stopped = AtomicBool::new(false);
        let removed = Mutex::new(Vec::new());
        delete_segments_with(
            &dir.path().join("out.m3u8"),
            1,
            &|path| {
                std::fs::remove_file(path)?;
                removed.lock().unwrap().push(path.to_owned());
                stopped.store(true, Ordering::Release);
                Ok(())
            },
            &|| stopped.load(Ordering::Acquire),
        )
        .unwrap();
        assert_eq!(removed.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn copy_input_pacing_has_version_and_native_rate_precedence() {
        let mut state = state();
        let mut options = EncodingOptions {
            enable_segment_deletion: true,
            ..EncodingOptions::default()
        };
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(4, 4))),
            Vec::<String>::new()
        );
        assert_eq!(
            input_rate_arguments(&state, &options, None),
            Vec::<String>::new()
        );
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(5, 0))),
            ["-readrate", "10"]
        );
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(7, 1))),
            ["-readrate", "10"]
        );
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))),
            ["-readrate", "10", "-readrate_catchup", "1000"]
        );
        state.media_source.read_at_native_framerate = true;
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))),
            ["-re", "-readrate_catchup", "100"]
        );
        state.media_source.protocol = MediaProtocol::Rtsp;
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))),
            ["-readrate", "10", "-readrate_catchup", "1000"]
        );
        options.enable_segment_deletion = false;
        assert!(input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))).is_empty());
        state.media_source.read_at_native_framerate = false;
        options.enable_segment_deletion = true;
        state.output_video_codec = Some("libx264".to_owned());
        assert!(input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))).is_empty());
        state.output_video_codec = Some("copy".to_owned());
        state.video_stream = None;
        assert!(input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))).is_empty());
        state.video_stream = Some(ferrofin_model::entities_media::MediaStream::default());
        state.transcoding_type = TranscodingJobType::Progressive;
        assert!(input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))).is_empty());
        state.media_source.protocol = MediaProtocol::File;
        state.media_source.read_at_native_framerate = true;
        options.enable_segment_deletion = false;
        assert_eq!(
            input_rate_arguments(&state, &options, Some(FfmpegVersion::new(8, 0))),
            ["-re", "-readrate_catchup", "100"]
        );
    }

    struct LiveConfig {
        options: Mutex<EncodingOptions>,
        fail: AtomicBool,
        reads: AtomicUsize,
    }

    #[async_trait]
    impl ServerConfigurationManager for LiveConfig {
        async fn get_encoding_options(
            &self,
        ) -> Result<EncodingOptions, ferrofin_traits::error::ServiceError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(ferrofin_traits::error::ServiceError::backend(
                    "test config failure",
                ));
            }
            Ok(self.options.lock().unwrap().clone())
        }
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unimplemented!("cleaner reads only encoding options")
        }
        async fn configuration(
            &self,
        ) -> Result<
            Arc<ferrofin_model::configuration::ServerConfiguration>,
            ferrofin_traits::error::ServiceError,
        > {
            unimplemented!("cleaner reads only encoding options")
        }
        async fn update_configuration(
            &self,
            _: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!("cleaner reads only encoding options")
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ferrofin_traits::error::ServiceError>
        {
            unimplemented!("cleaner reads only encoding options")
        }
        async fn update_branding(
            &self,
            _: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!("cleaner reads only encoding options")
        }
    }

    async fn child(dir: &Path) -> Arc<dyn TranscodeChild> {
        let fake = FakeSegmentTranscoder::new(FakeScript::default());
        Arc::from(
            fake.start_transcode(&SpawnRequest {
                program: "fake".to_owned(),
                arguments: Vec::new(),
                working_dir: None,
                output_dir: dir.to_owned(),
                log_path: dir.join("stderr.log"),
                env: Vec::new(),
            })
            .await
            .unwrap(),
        )
    }

    async fn wait_until_deleted(path: &Path) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_for_config_reads(config: &LiveConfig, minimum: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while config.reads.load(Ordering::SeqCst) < minimum {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dropping_a_timer_cancels_a_pending_deletion_delay() {
        let dir = tempfile::tempdir().unwrap();
        let segment = dir.path().join("out0.ts");
        std::fs::write(&segment, b"segment").unwrap();
        let config = Arc::new(LiveConfig {
            options: Mutex::new(EncodingOptions {
                enable_segment_deletion: true,
                segment_keep_seconds: 20,
                ..EncodingOptions::default()
            }),
            fail: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        });
        let process = child(dir.path()).await;
        let selected = Arc::new(AtomicBool::new(false));
        let selected_by_worker = Arc::clone(&selected);
        let timer = SegmentCleanerTask::start_with_timing(
            process,
            config,
            dir.path().join("out.m3u8"),
            6,
            Arc::new(move || {
                selected_by_worker.store(true, Ordering::Release);
                Some(400_000_000)
            }),
            Duration::from_millis(10),
            Duration::from_secs(3600),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !selected.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            segment.is_file(),
            "the selected batch is still awaiting its deletion delay"
        );
        let cancellation = timer.task.abort_handle();
        let stopped = Arc::clone(&timer.stopped);
        drop(timer);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cancellation.is_finished() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert!(stopped.load(Ordering::Acquire));
        assert!(
            segment.is_file(),
            "teardown cancels the batch before its first removal"
        );
    }

    #[tokio::test]
    async fn owned_timer_reloads_live_options_and_stops_on_drop_or_process_exit() {
        let dir = tempfile::tempdir().unwrap();
        let segment = dir.path().join("out0.ts");
        std::fs::write(&segment, b"segment").unwrap();
        let config = Arc::new(LiveConfig {
            options: Mutex::new(EncodingOptions {
                segment_keep_seconds: 20,
                ..EncodingOptions::default()
            }),
            fail: AtomicBool::new(true),
            reads: AtomicUsize::new(0),
        });
        let process = child(dir.path()).await;
        let timer = SegmentCleanerTask::start_with_timing(
            Arc::clone(&process),
            config.clone(),
            dir.path().join("out.m3u8"),
            6,
            Arc::new(|| Some(400_000_000)),
            Duration::from_millis(10),
            Duration::from_millis(2),
        );
        wait_for_config_reads(&config, 2).await;
        assert!(
            segment.exists(),
            "failed option reads cannot schedule deletion"
        );
        config.fail.store(false, Ordering::SeqCst);
        let disabled_read = config.reads.load(Ordering::SeqCst) + 1;
        wait_for_config_reads(&config, disabled_read).await;
        assert!(
            segment.exists(),
            "disabled setting retains finalized segments"
        );
        config.options.lock().unwrap().enable_segment_deletion = true;
        wait_until_deleted(&segment).await;
        let cancellation = timer.task.abort_handle();
        drop(timer);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cancellation.is_finished() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(&segment, b"segment").unwrap();
        assert!(segment.exists(), "dropped timer must stop deleting");
        process.kill().await.unwrap();
        let reads_before_exit = config.reads.load(Ordering::SeqCst);
        let timer = SegmentCleanerTask::start_with_timing(
            process,
            config.clone(),
            dir.path().join("out.m3u8"),
            6,
            Arc::new(|| Some(400_000_000)),
            Duration::from_millis(10),
            Duration::from_millis(2),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !timer.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert!(segment.exists(), "an exited job must not delete");
        assert_eq!(
            config.reads.load(Ordering::SeqCst),
            reads_before_exit,
            "an exited worker must stop before reading options"
        );
    }
}
