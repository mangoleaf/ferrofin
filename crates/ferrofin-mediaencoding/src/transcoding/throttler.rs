//! Transcode pause policy from Jellyfin's `TranscodingThrottler`.

use crate::encoder::FfmpegVersion;
use crate::encoding_helper::EncodingJobInfo;
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::VideoType;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::media_encoding::TranscodingProgress;

/// How often an eligible job checks the current throttle policy.
pub const THROTTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// .NET ticks per second.
pub const TICKS_PER_SECOND: i64 = 10_000_000;
/// The minimum runtime eligible for throttling (five minutes).
pub const MINIMUM_RUNTIME_TICKS: i64 = 300 * TICKS_PER_SECOND;

/// Keyboard controls supported by the detected ffmpeg binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseKeys {
    /// Jellyfin-ffmpeg's explicit pause (`p`) and resume (`u`) keys.
    Patched,
    /// Old ffmpeg's interactive command prompt (`c`) and newline.
    Legacy,
}

impl PauseKeys {
    /// Selects the exact upstream capability/version gate.
    ///
    /// `6.1.1` sorts after `Version(6, 1)` and does not take the legacy path.
    #[must_use]
    pub fn select(pkey_supported: bool, version: Option<FfmpegVersion>) -> Option<Self> {
        if pkey_supported {
            Some(Self::Patched)
        } else if version.is_some_and(|version| version <= FfmpegVersion::new(6, 1)) {
            Some(Self::Legacy)
        } else {
            None
        }
    }

    /// The key that pauses the encoder.
    #[must_use]
    pub const fn pause(self) -> &'static [u8] {
        match self {
            Self::Patched => b"p",
            Self::Legacy => b"c",
        }
    }

    /// The key that resumes the encoder.
    #[must_use]
    pub const fn resume(self) -> &'static [u8] {
        match self {
            Self::Patched => b"u",
            Self::Legacy => {
                if cfg!(windows) {
                    b"\r\n"
                } else {
                    b"\n"
                }
            }
        }
    }
}

/// The progress shared by the encoder and its HTTP consumers.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ThrottleProgress {
    /// The current ffmpeg progress, with the job's seek offset applied.
    pub encoded: TranscodingProgress,
    /// The greatest ending position of a completed dynamic HLS segment.
    pub download_position_ticks: Option<i64>,
    /// Bytes read from a progressive transcode by HTTP responses.
    pub bytes_downloaded: i64,
}

/// Whether the input passes Jellyfin's `EnableThrottling(StreamState)` gate.
#[must_use]
pub fn eligible(state: &EncodingJobInfo) -> bool {
    // Pinned StreamingHelpers/AttachMediaSourceInfo never assign the separate
    // EncodingJobInfo.VideoType property. Its default is VideoFile even when
    // MediaSource.VideoType says Dvd/BluRay; using that source field here would
    // make the ordinary API production gate stricter than Jellyfin's.
    eligible_input(
        effective_input_protocol(state),
        state.run_time_ticks,
        state.is_input_video,
        VideoType::VideoFile,
    )
}

/// The effective input protocol after the optional encoder-path override.
#[must_use]
pub fn effective_input_protocol(state: &EncodingJobInfo) -> MediaProtocol {
    if state
        .media_source
        .encoder_path
        .as_deref()
        .is_some_and(|path| !path.is_empty())
    {
        state
            .media_source
            .encoder_protocol
            .unwrap_or(state.media_source.protocol)
    } else {
        state.media_source.protocol
    }
}

/// The upstream gate for explicitly supplied encoding-state values.
#[must_use]
pub fn eligible_input(
    protocol: MediaProtocol,
    runtime: Option<i64>,
    input_video: bool,
    video_type: VideoType,
) -> bool {
    protocol == MediaProtocol::File
        && runtime.is_some_and(|ticks| ticks >= MINIMUM_RUNTIME_TICKS)
        && input_video
        && video_type == VideoType::VideoFile
}

/// Whether the encoder is at least the configured delay ahead of its consumer.
///
/// `output_length` supplies the filesystem fallback when stderr reports no
/// encoded byte count; failure to obtain it leaves the encoder running.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "the upstream byte estimate intentionally uses double arithmetic"
)]
pub fn should_pause(
    options: &EncodingOptions,
    progress: ThrottleProgress,
    output_length: Option<i64>,
) -> bool {
    if !options.enable_throttling {
        return false;
    }
    let encoded_ticks = progress.encoded.position_ticks.unwrap_or(0);
    let downloaded_ticks = progress.download_position_ticks.unwrap_or(0);
    let gap_ticks = i64::from(options.throttle_delay_seconds.max(60)) * TICKS_PER_SECOND;
    if downloaded_ticks > 0 && encoded_ticks > 0 {
        return encoded_ticks.saturating_sub(downloaded_ticks) >= gap_ticks;
    }
    if progress.bytes_downloaded > 0 && encoded_ticks > 0 {
        let Some(encoded_bytes) = progress.encoded.bytes_transcoded.or(output_length) else {
            return false;
        };
        let target_gap = encoded_bytes as f64 * (gap_ticks as f64 / encoded_ticks as f64);
        let actual_gap = encoded_bytes.saturating_sub(progress.bytes_downloaded) as f64;
        return actual_gap >= target_gap;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_capability_and_exact_version_gate() {
        for (patched, version, expected) in [
            (true, None, Some(PauseKeys::Patched)),
            (
                true,
                Some(FfmpegVersion::new(8, 0)),
                Some(PauseKeys::Patched),
            ),
            (false, None, None),
            (
                false,
                Some(FfmpegVersion::new(6, 0)),
                Some(PauseKeys::Legacy),
            ),
            (
                false,
                Some(FfmpegVersion::new(6, 1)),
                Some(PauseKeys::Legacy),
            ),
            (false, Some(FfmpegVersion::with_build(6, 1, 0)), None),
            (false, Some(FfmpegVersion::with_build(6, 1, 1)), None),
            (false, Some(FfmpegVersion::new(7, 0)), None),
        ] {
            assert_eq!(PauseKeys::select(patched, version), expected);
        }
        assert_eq!(PauseKeys::Patched.pause(), b"p");
        assert_eq!(PauseKeys::Patched.resume(), b"u");
        assert_eq!(PauseKeys::Legacy.pause(), b"c");
        assert_eq!(
            PauseKeys::Legacy.resume(),
            if cfg!(windows) {
                &b"\r\n"[..]
            } else {
                &b"\n"[..]
            }
        );
    }

    #[test]
    fn eligible_inputs_are_local_known_video_files_of_at_least_five_minutes() {
        for ticks in [None, Some(-1), Some(0), Some(MINIMUM_RUNTIME_TICKS - 1)] {
            assert!(!eligible_input(
                MediaProtocol::File,
                ticks,
                true,
                VideoType::VideoFile
            ));
        }
        assert!(eligible_input(
            MediaProtocol::File,
            Some(MINIMUM_RUNTIME_TICKS),
            true,
            VideoType::VideoFile
        ));
        assert!(eligible_input(
            MediaProtocol::File,
            Some(MINIMUM_RUNTIME_TICKS + 1),
            true,
            VideoType::VideoFile
        ));
        for protocol in [
            MediaProtocol::Http,
            MediaProtocol::Rtsp,
            MediaProtocol::Rtmp,
            MediaProtocol::Rtp,
            MediaProtocol::Ftp,
            MediaProtocol::Udp,
        ] {
            assert!(!eligible_input(
                protocol,
                Some(MINIMUM_RUNTIME_TICKS),
                true,
                VideoType::VideoFile
            ));
        }
        for video_type in [
            VideoType::Dvd,
            VideoType::BluRay,
            VideoType::Iso,
            VideoType::Unrecognized(99),
        ] {
            assert!(!eligible_input(
                MediaProtocol::File,
                Some(MINIMUM_RUNTIME_TICKS),
                true,
                video_type
            ));
        }
        assert!(!eligible_input(
            MediaProtocol::File,
            Some(MINIMUM_RUNTIME_TICKS),
            false,
            VideoType::VideoFile
        ));
    }

    fn options(delay: i32) -> EncodingOptions {
        EncodingOptions {
            enable_throttling: true,
            throttle_delay_seconds: delay,
            ..EncodingOptions::default()
        }
    }

    #[test]
    fn hls_exact_threshold_clamp_disabled_and_missing_progress() {
        let mut p = ThrottleProgress {
            download_position_ticks: Some(TICKS_PER_SECOND),
            ..ThrottleProgress::default()
        };
        for delay in [i32::MIN, -1, 0, 1, 59, 60] {
            p.encoded.position_ticks = Some(61 * TICKS_PER_SECOND - 1);
            assert!(!should_pause(&options(delay), p, None));
            p.encoded.position_ticks = Some(61 * TICKS_PER_SECOND);
            assert!(should_pause(&options(delay), p, None));
        }
        p.encoded.position_ticks = Some(181 * TICKS_PER_SECOND);
        assert!(should_pause(&options(180), p, None));
        assert!(!should_pause(&options(181), p, None));
        assert!(!should_pause(&EncodingOptions::default(), p, None));
        for downloaded in [None, Some(0), Some(-1)] {
            p.download_position_ticks = downloaded;
            assert!(!should_pause(&options(60), p, None));
        }
        p.download_position_ticks = Some(TICKS_PER_SECOND);
        for encoded in [None, Some(0), Some(-1)] {
            p.encoded.position_ticks = encoded;
            assert!(!should_pause(&options(60), p, None));
        }
    }

    #[test]
    fn progressive_estimate_prefers_reported_bytes_and_fails_open_on_stat_error() {
        let mut p = ThrottleProgress {
            bytes_downloaded: 400,
            ..ThrottleProgress::default()
        };
        p.encoded.position_ticks = Some(100 * TICKS_PER_SECOND);
        assert!(should_pause(&options(60), p, Some(1000)));
        p.bytes_downloaded = 401;
        assert!(!should_pause(&options(60), p, Some(1000)));
        assert!(!should_pause(&options(60), p, None));
        p.encoded.bytes_transcoded = Some(2000);
        assert!(should_pause(&options(60), p, Some(1000)));
        p.bytes_downloaded = 801;
        assert!(!should_pause(&options(60), p, Some(9000)));
        p.bytes_downloaded = 0;
        assert!(!should_pause(&options(60), p, Some(9000)));
    }
}
