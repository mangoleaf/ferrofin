//! The [`MediaEncoderImpl`] — the `ferrofin-traits` [`MediaEncoder`] implementation.
//!
//! Port of the object-safe, domain-tree-free subset of
//! `MediaBrowser.MediaEncoding.Encoder.MediaEncoder`: the `EncoderPath`/`ProbePath`
//! accessors, `SetFFmpegPath`, `GetMediaInfo` (ffprobe → [`MediaSourceInfo`] via
//! the ported [`ProbeResultNormalizer`]), `GetInputArgument`, `GetTimeParameter`,
//! the `Extract*Image` frame grabbers, `ConvertImage`, and the tested
//! `GetExtraArguments` User-Agent/probe oracle.
//!
//! The hardware-acceleration matrix and HDR tonemapping are the work items of
//! `brain/plans/PLAN_HWACCEL.md` (this unit gains its accelerated trickplay
//! path in that plan's phase 9); Blu-ray (`BdInfo`) has no plan yet and belongs
//! to disc-image playback rather than encoding. Every ffmpeg/ffprobe process
//! spawn sits behind the [`Transcoder`] seam so unit tests inject a fake.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::RwLock;

use crate::error::MediaEncodingError;
use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{IsoType, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::{MediaInfo, MediaProtocol};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

use super::Transcoder;
use super::encoding_utils::get_input_argument;
use crate::probing::dtos::InternalMediaInfoResult;
use crate::probing::localization::PassthroughLocalization;
use crate::probing::probe_result_normalizer::ProbeResultNormalizer;

/// The number of 100-nanosecond ticks in one second (`TimeSpan.TicksPerSecond`).
#[cfg(test)]
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Tunable knobs read from the encoding configuration.
///
/// Groups the handful of `IConfiguration`/`EncodingOptions`-derived values the
/// ported argument builders read (`GetFFmpegAnalyzeDuration`,
/// `GetFFmpegProbeSize`, thread count), so the encoder stays free of a full
/// configuration-manager dependency.
#[derive(Debug, Clone, Default)]
pub struct MediaEncoderConfig {
    /// The `-analyzeduration` value applied when the source has none
    /// (`GetFFmpegAnalyzeDuration`); `None` omits the flag.
    pub analyze_duration: Option<String>,
    /// The `-probesize` value (`GetFFmpegProbeSize`); `None` omits the flag.
    pub probe_size: Option<String>,
    /// The `-threads` count passed to ffmpeg/ffprobe. `0` lets ffmpeg decide.
    pub threads: i32,
    /// Where single-frame extractions write their temporary output (the C#
    /// `TempDirectory`). Empty (the `Default`) falls back to the OS temp dir.
    /// Must be a server-writable path: media directories are often read-only
    /// mounts, so extraction output can never go next to the input file.
    pub temp_dir: std::path::PathBuf,
}

/// The resolved ffmpeg/ffprobe binary paths.
#[derive(Debug, Clone, Default)]
struct Paths {
    ffmpeg: String,
    ffprobe: String,
}

/// The `ferrofin-traits` [`MediaEncoder`] implementation.
///
/// Generic over the [`Transcoder`] seam (real ffmpeg spawn vs. a test fake).
pub struct MediaEncoderImpl<T: Transcoder> {
    transcoder: Arc<T>,
    config: MediaEncoderConfig,
    paths: RwLock<Paths>,
    normalizer: ProbeResultNormalizer<PassthroughLocalization>,
}

impl<T: Transcoder> MediaEncoderImpl<T> {
    /// Creates an encoder using `transcoder` for process invocation and the
    /// given `ffmpeg`/`ffprobe` paths and `config`.
    pub fn new(
        transcoder: Arc<T>,
        ffmpeg_path: impl Into<String>,
        ffprobe_path: impl Into<String>,
        config: MediaEncoderConfig,
    ) -> Self {
        Self {
            transcoder,
            config,
            paths: RwLock::new(Paths {
                ffmpeg: ffmpeg_path.into(),
                ffprobe: ffprobe_path.into(),
            }),
            normalizer: ProbeResultNormalizer::new(PassthroughLocalization),
        }
    }

    /// Builds the extra ffprobe arguments (analyze-duration, probe-size,
    /// per-source `User-Agent`, and RTSP transport).
    ///
    /// Port of `MediaEncoder.GetExtraArguments`. This is the method exercised by
    /// the `ProbeExternalSourcesTests` oracle.
    #[must_use]
    pub fn get_extra_arguments(&self, request: &MediaInfoRequest) -> String {
        let ffmpeg_analyze_duration = self.config.analyze_duration.as_deref().unwrap_or_default();
        let ffmpeg_probe_size = self.config.probe_size.as_deref().unwrap_or_default();
        let source = &request.media_source;

        let mut analyze_duration = String::new();
        if source.analyze_duration_ms.unwrap_or(0) > 0 {
            // C# multiplies the millisecond value by 1000 (into microseconds).
            let micros = i64::from(source.analyze_duration_ms.unwrap_or(0)) * 1000;
            analyze_duration = format!("-analyzeduration {micros}");
        } else if !ffmpeg_analyze_duration.is_empty() {
            analyze_duration = format!("-analyzeduration {ffmpeg_analyze_duration}");
        }

        let mut extra_args = String::new();
        if !analyze_duration.is_empty() {
            extra_args = analyze_duration;
        }

        if !ffmpeg_probe_size.is_empty() {
            let _ = write!(extra_args, " -probesize {ffmpeg_probe_size}");
        }

        if let Some(user_agent) = source.required_http_headers.get("User-Agent") {
            let _ = write!(extra_args, " -user_agent \"{user_agent}\"");
        }

        if source.protocol == MediaProtocol::Rtsp {
            extra_args += " -rtsp_transport tcp+udp -rtsp_flags prefer_tcp";
        }

        extra_args
    }

    /// Formats a `TimeSpan` (given as ticks) as ffmpeg's `hh:mm:ss.fff`.
    ///
    /// Port of `GetTimeParameter(TimeSpan)`; kept as a free helper so the image
    /// builders can prepend an `-ss` seek.
    fn format_time_parameter(ticks: i64) -> String {
        let negative = ticks < 0;
        let total_ms = ticks.unsigned_abs() / 10_000; // 100ns ticks -> ms
        let ms = total_ms % 1000;
        let total_seconds = total_ms / 1000;
        let seconds = total_seconds % 60;
        let minutes = (total_seconds / 60) % 60;
        let hours = total_seconds / 3600;
        let sign = if negative { "-" } else { "" };
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}.{ms:03}")
    }

    /// Builds the ffprobe argument line for a probe (mirrors
    /// `GetMediaInfoInternal`'s format string, minus the deferred
    /// first-video-frame probe).
    fn probe_arguments(&self, input_path: &str, extract_chapters: bool, extra: &str) -> String {
        let template = if extract_chapters {
            "-i {input} -threads {threads} -v warning -print_format json -show_streams -show_chapters -show_format"
        } else {
            "-i {input} -threads {threads} -v warning -print_format json -show_streams -show_format"
        };
        let body = template
            .replace("{input}", input_path)
            .replace("{threads}", &self.config.threads.to_string());
        let combined = if extra.is_empty() {
            body
        } else {
            format!("{extra} {body}")
        };
        combined.trim().to_owned()
    }

    /// Builds the `ExtractImageInternal` argument line (software path only).
    ///
    /// Port of the core of `ExtractImageInternal`: the deinterlace + scale
    /// filter chain, optional thumbnail sampling, stream `-map`, and `-ss`
    /// offset.
    ///
    /// Two branches of the C# are missing here and are tracked separately: the
    /// HDR tonemap branch belongs to `brain/plans/PLAN_HWACCEL.md` phase 2, and
    /// `GetImageResolutionParameter` (the `ChapterImageResolution` setting →
    /// `-s WxH`) is not hardware work at all — it is open work item 4 in that
    /// plan's list, because the dashboard setting currently has no effect.
    #[allow(clippy::too_many_arguments)]
    fn extract_image_arguments(
        input_path: &str,
        container: &str,
        video_stream: Option<&MediaStream>,
        image_stream_index: Option<i32>,
        threed_format: Option<Video3DFormat>,
        offset_ticks: Option<i64>,
        use_iframe: bool,
        output_path: &str,
        threads: i32,
    ) -> String {
        let mut filters: Vec<String> = Vec::new();

        if video_stream.is_some_and(|s| s.is_interlaced) {
            filters.push("bwdif=0:-1:0".to_owned());
        }

        let scaler = match threed_format {
            Some(Video3DFormat::HalfSideBySide) => {
                "crop=iw/2:ih:0:0,scale=(iw*2):ih,setdar=dar=a,crop=min(iw\\,ih*dar):min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1"
            }
            Some(Video3DFormat::FullSideBySide) => {
                "crop=iw/2:ih:0:0,setdar=dar=a,crop=min(iw\\,ih*dar):min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1"
            }
            Some(Video3DFormat::HalfTopAndBottom) => {
                // Accepted divergence: upstream writes `scale=(iw*2):ih)` here,
                // with one closing bracket too many. ffmpeg's expression parser
                // rejects it (`[Eval] Invalid chars ')' at the end of expression 'ih)'`) and the extraction fails,
                // so chapter and thumbnail images cannot be produced for this
                // 3D layout on Jellyfin. Verified both forms against ffmpeg
                // n9.0.1. The balanced form below is what the sibling
                // half-side-by-side case two arms up already has.
                "crop=iw:ih/2:0:0,scale=(iw*2):ih,setdar=dar=a,crop=min(iw\\,ih*dar):min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1"
            }
            Some(Video3DFormat::FullTopAndBottom) => {
                "crop=iw:ih/2:0:0,setdar=dar=a,crop=min(iw\\,ih*dar):min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1"
            }
            _ => "scale=round(iw*sar/2)*2:round(ih/2)*2",
        };
        filters.push(scaler.to_owned());

        let enable_thumbnail = use_iframe && !container.eq_ignore_ascii_case("wtv");
        if enable_thumbnail {
            filters.push("thumbnail=n=24".to_owned());
        }

        let vf = filters.join(",");
        let map_arg = image_stream_index
            .map(|i| format!(" -map 0:{i}"))
            .unwrap_or_default();

        // `-v error`, not upstream's `-v quiet`: the only signal a failed
        // extraction gives is a missing output file, and the caller reports
        // ffmpeg's stderr tail to explain why. Under `-v quiet` that tail is
        // always empty — an unreadable input, an unwritable output directory
        // and a truly frameless offset all render as the same blank message.
        // `error` adds nothing on the happy path.
        let mut args = format!(
            "-i {input_path}{map_arg} -threads {threads} -v error -vframes 1 -vf {vf} -f image2 \"{output_path}\""
        );

        if let Some(offset) = offset_ticks {
            args = format!("-ss {} {args}", Self::format_time_parameter(offset));
        }

        let seek_mpegts = offset_ticks.is_some() && container.eq_ignore_ascii_case("mpegts");
        if use_iframe && seek_mpegts {
            args = format!("-skip_frame nokey {args}");
        }

        args
    }

    /// Parses ffprobe's captured JSON into an [`InternalMediaInfoResult`].
    fn parse_probe(output: &str) -> Result<InternalMediaInfoResult, ServiceError> {
        serde_json::from_str(output).map_err(|e| MediaEncodingError::probe_parse(e).into())
    }
}

#[async_trait]
impl<T: Transcoder> MediaEncoder for MediaEncoderImpl<T> {
    fn encoder_path(&self) -> String {
        self.paths
            .read()
            .expect("paths lock poisoned")
            .ffmpeg
            .clone()
    }

    fn probe_path(&self) -> String {
        self.paths
            .read()
            .expect("paths lock poisoned")
            .ffprobe
            .clone()
    }

    async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
        // Port of `SetFFmpegPath`, reduced to a version validation of the
        // configured binary. The C# directory search + fallback download are
        // host concerns; here a valid path is one whose `-version` invocation
        // succeeds.
        let path = self.encoder_path();
        if path.trim().is_empty() {
            return Ok(false);
        }
        Ok(self
            .transcoder
            .get_process_exit_code(&path, "-version")
            .await)
    }

    async fn get_media_info(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        // The source (with chapters) is the media_source slice of the full probe.
        Ok(self.get_media_info_full(request).await?.media_source)
    }

    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        let source = &request.media_source;
        let input_file = source
            .path
            .as_deref()
            .ok_or_else(|| ServiceError::invalid_input("media source has no path"))?;

        let prefix = if source.iso_type == Some(IsoType::BluRay) {
            "bluray"
        } else {
            "file"
        };
        let input_path = get_input_argument(prefix, input_file, source.protocol);
        let extra = self.get_extra_arguments(request);
        let args = self.probe_arguments(&input_path, request.extract_chapters, &extra);

        let probe = self.probe_path();
        let output = self
            .transcoder
            .get_process_output(&probe, &args, false, None)
            .await
            .map_err(MediaEncodingError::process)?;

        let data = Self::parse_probe(&output)?;
        let mut info = self.normalizer.get_media_info(
            data,
            source.video_type,
            request.media_is_audio,
            input_file,
            source.protocol,
        );
        // Carry the probed chapters on the source (internal-only field) so the
        // scan can persist them; they were requested via `extract_chapters`.
        info.media_source.chapters = info.chapters.clone();
        Ok(info)
    }

    async fn extract_audio_image(
        &self,
        path: &str,
        image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        let output_path = format!("{path}.image.jpg");
        let args = Self::extract_image_arguments(
            &format!("file:\"{path}\""),
            "",
            None,
            image_stream_index,
            None,
            None,
            false,
            &output_path,
            self.config.threads,
        );
        let ffmpeg = self.encoder_path();
        self.transcoder
            .get_process_output(&ffmpeg, &args, true, None)
            .await
            .map_err(MediaEncodingError::process)?;
        Ok(output_path)
    }

    async fn extract_video_image(
        &self,
        input_file: &str,
        container: &str,
        media_source: &MediaSourceInfo,
        video_stream: &MediaStream,
        threed_format: Option<Video3DFormat>,
        offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        // Write the frame into the server's temp dir (the C# `TempDirectory` +
        // `Guid.NewGuid()` shape), never next to the input: media is routinely
        // a read-only mount, and even a writable library must not accumulate
        // extraction droppings beside the files.
        let temp_dir = if self.config.temp_dir.as_os_str().is_empty() {
            std::env::temp_dir()
        } else {
            self.config.temp_dir.clone()
        };
        // Creating the directory is not enough to know ffmpeg can write into
        // it: a pre-existing one owned by another user (a container that once
        // ran as root, then dropped to an unprivileged uid) accepts
        // `create_dir_all` and refuses every write. ffmpeg would then produce
        // no file for every chapter of every video, which reads as "this media
        // has no extractable frame". Name the real problem, once, here.
        ferrofin_util::file_helper::ensure_writable_dir(&temp_dir).map_err(|e| {
            MediaEncodingError::process(format!(
                "frame-extraction temp directory `{}` is not writable: {e}",
                temp_dir.display()
            ))
        })?;
        let output_path = temp_dir
            .join(format!(
                "ferrofin-extract-{}.jpg",
                uuid::Uuid::new_v4().simple()
            ))
            .to_string_lossy()
            .into_owned();
        let input_path = self.get_input_argument(input_file, media_source);
        let image_stream_index = if video_stream.stream_type == MediaStreamType::Video {
            None
        } else {
            Some(video_stream.index)
        };
        let args = Self::extract_image_arguments(
            &input_path,
            container,
            Some(video_stream),
            image_stream_index,
            threed_format,
            offset_ticks,
            true,
            &output_path,
            self.config.threads,
        );
        let ffmpeg = self.encoder_path();
        let stderr = self
            .transcoder
            .get_process_output(&ffmpeg, &args, true, None)
            .await
            .map_err(MediaEncodingError::process)?;
        // The process runner mirrors the C# `GetProcessOutput` and ignores the
        // exit code, so a failed ffmpeg (unreadable input, no frame at the
        // offset) surfaces only as a missing output file — check it here so
        // the caller gets the real story instead of an ENOENT from its own
        // move of a file that never existed.
        if !std::path::Path::new(&output_path).exists() {
            let tail: String = stderr
                .chars()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .iter()
                .rev()
                .collect();
            return Err(MediaEncodingError::process(format!(
                "ffmpeg produced no frame from `{input_file}`: {}",
                if tail.trim().is_empty() {
                    "(no stderr)"
                } else {
                    tail.trim()
                }
            ))
            .into());
        }
        Ok(output_path)
    }

    fn get_input_argument(&self, input_file: &str, media_source: &MediaSourceInfo) -> String {
        let prefix = if media_source.iso_type == Some(IsoType::BluRay) {
            "bluray"
        } else {
            "file"
        };
        get_input_argument(prefix, input_file, media_source.protocol)
    }

    fn get_time_parameter(&self, ticks: i64) -> String {
        Self::format_time_parameter(ticks)
    }

    async fn convert_image(
        &self,
        _input_path: &str,
        _output_path: &str,
    ) -> Result<(), ServiceError> {
        // Port of `ConvertImage`, which is `throw new NotImplementedException()`
        // in upstream Jellyfin.
        Err(ServiceError::backend("ConvertImage is not implemented"))
    }
}

#[cfg(test)]
mod tests {
    use ferrofin_model::entities::Video3DFormat;
    use rstest::rstest;

    use std::sync::Arc;

    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::media_info::MediaProtocol;
    use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

    use super::{MediaEncoderConfig, MediaEncoderImpl, TICKS_PER_SECOND};
    use crate::encoder::Transcoder;
    use async_trait::async_trait;

    /// A [`Transcoder`] fake that never spawns a process.
    struct NoopTranscoder;

    #[async_trait]
    impl Transcoder for NoopTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            _arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
        ) -> Result<String, String> {
            Ok(String::new())
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    fn encoder() -> MediaEncoderImpl<NoopTranscoder> {
        MediaEncoderImpl::new(
            Arc::new(NoopTranscoder),
            "/usr/bin/ffmpeg",
            "/usr/bin/ffprobe",
            MediaEncoderConfig::default(),
        )
    }

    fn http_source_with_user_agent(user_agent: &str) -> MediaSourceInfo {
        let mut source = MediaSourceInfo {
            path: Some("/path/to/stream".to_owned()),
            protocol: MediaProtocol::Http,
            ..MediaSourceInfo::default()
        };
        source
            .required_http_headers
            .insert("User-Agent".to_owned(), user_agent.to_owned());
        source
    }

    /// Verbatim transliteration of `ProbeExternalSourcesTests`
    /// `GetExtraArguments_Forwards_UserAgent`.
    #[test]
    fn get_extra_arguments_forwards_user_agent() {
        let enc = encoder();
        let user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
        let request = MediaInfoRequest {
            media_source: http_source_with_user_agent(user_agent),
            extract_chapters: false,
            media_is_audio: false,
        };

        let extra_arg = enc.get_extra_arguments(&request);

        assert!(extra_arg.contains(&format!("-user_agent \"{user_agent}\"")));
    }

    #[test]
    fn rtsp_source_adds_transport_flags() {
        let enc = encoder();
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some("rtsp://host/stream".to_owned()),
                protocol: MediaProtocol::Rtsp,
                ..MediaSourceInfo::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        let extra = enc.get_extra_arguments(&request);
        assert!(extra.contains("-rtsp_transport tcp+udp -rtsp_flags prefer_tcp"));
    }

    #[test]
    fn analyze_duration_ms_is_converted_to_microseconds() {
        let enc = encoder();
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some("/x".to_owned()),
                protocol: MediaProtocol::File,
                analyze_duration_ms: Some(200),
                ..MediaSourceInfo::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        assert_eq!(enc.get_extra_arguments(&request), "-analyzeduration 200000");
    }

    #[test]
    fn time_parameter_formats_hh_mm_ss_fff() {
        let enc = encoder();
        // 1h 2m 3.456s
        let ticks = (3600 + 2 * 60 + 3) * TICKS_PER_SECOND + 456 * 10_000;
        assert_eq!(enc.get_time_parameter(ticks), "01:02:03.456");
        assert_eq!(enc.get_time_parameter(0), "00:00:00.000");
    }

    #[test]
    fn input_argument_uses_bluray_prefix_for_bluray_iso() {
        let enc = encoder();
        let source = MediaSourceInfo {
            protocol: MediaProtocol::File,
            iso_type: Some(ferrofin_model::entities::IsoType::BluRay),
            ..MediaSourceInfo::default()
        };
        assert_eq!(
            enc.get_input_argument("/media/movie", &source),
            "bluray:\"/media/movie\""
        );
    }

    #[test]
    fn encoder_and_probe_paths_are_reported() {
        let enc = encoder();
        assert_eq!(enc.encoder_path(), "/usr/bin/ffmpeg");
        assert_eq!(enc.probe_path(), "/usr/bin/ffprobe");
    }

    #[tokio::test]
    async fn convert_image_reports_not_implemented() {
        let enc = encoder();
        assert!(enc.convert_image("/a", "/b").await.is_err());
    }

    #[tokio::test]
    async fn set_ffmpeg_path_true_for_valid_binary() {
        let enc = encoder();
        assert!(enc.set_ffmpeg_path().await.unwrap());
    }

    /// A [`Transcoder`] fake that "extracts a frame": it parses the quoted
    /// output path off the argument tail (the shape `extract_image_arguments`
    /// emits) and writes a stub file there, like a successful ffmpeg run.
    struct FrameWritingTranscoder;

    #[async_trait]
    impl Transcoder for FrameWritingTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
        ) -> Result<String, String> {
            let out = arguments
                .rsplit('"')
                .nth(1)
                .expect("quoted output path is the last argument");
            std::fs::write(out, b"jpg").expect("write frame");
            Ok(String::new())
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    fn video_stream() -> ferrofin_model::entities_media::MediaStream {
        ferrofin_model::entities_media::MediaStream {
            stream_type: ferrofin_model::entities::MediaStreamType::Video,
            index: 0,
            ..ferrofin_model::entities_media::MediaStream::default()
        }
    }

    // The extraction writes into the configured temp dir — NEVER next to the
    // input file, which is routinely on a read-only mount (the production
    // chapter-image task failed on every item because of exactly that).
    #[tokio::test]
    async fn extract_video_image_writes_to_the_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let enc = MediaEncoderImpl::new(
            Arc::new(FrameWritingTranscoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: tmp.path().to_path_buf(),
                ..MediaEncoderConfig::default()
            },
        );
        let out = enc
            .extract_video_image(
                "/read-only/media/episode.mkv",
                "",
                &MediaSourceInfo::default(),
                &video_stream(),
                None,
                Some(10 * TICKS_PER_SECOND),
            )
            .await
            .expect("extraction succeeds");
        assert!(
            std::path::Path::new(&out).starts_with(tmp.path()),
            "frame must land in the temp dir, got {out}"
        );
        assert!(std::path::Path::new(&out).exists());
        assert!(
            !out.starts_with("/read-only/media"),
            "must not write next to the input"
        );
    }

    // The process runner ignores ffmpeg's exit code (C# GetProcessOutput
    // parity), so a run that produced no file must be surfaced here as a real
    // error — not deferred to the caller's move of a nonexistent file.
    #[tokio::test]
    async fn extract_video_image_errors_when_no_frame_was_produced() {
        let tmp = tempfile::tempdir().unwrap();
        let enc = MediaEncoderImpl::new(
            Arc::new(NoopTranscoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: tmp.path().to_path_buf(),
                ..MediaEncoderConfig::default()
            },
        );
        let err = enc
            .extract_video_image(
                "/read-only/media/episode.mkv",
                "",
                &MediaSourceInfo::default(),
                &video_stream(),
                None,
                None,
            )
            .await
            .expect_err("no frame → error");
        assert!(
            err.to_string().contains("produced no frame"),
            "error names the real failure, got: {err}"
        );
    }

    // A failed extraction is only ever reported as a missing output file, and
    // the error carries ffmpeg's stderr tail to say why. Under `-v quiet`
    // ffmpeg writes nothing to stderr, so an unreadable input, an unwritable
    // output directory and a genuinely frameless offset all render as the same
    // blank "(no stderr)" — which is what made a real chapter-image outage
    // undiagnosable. The verbosity is load-bearing, so pin it.
    #[test]
    fn extraction_asks_ffmpeg_for_its_errors() {
        let args = MediaEncoderImpl::<NoopTranscoder>::extract_image_arguments(
            "file:\"/media/episode.mkv\"",
            "",
            None,
            None,
            None,
            None,
            true,
            "/tmp/out.jpg",
            0,
        );
        assert!(args.contains("-v error"), "got: {args}");
        assert!(!args.contains("-v quiet"), "got: {args}");
    }

    #[rstest]
    // The five scaler shapes `ExtractImageInternal` chooses between. These
    // strings are the only 3D handling on a path that actually ships — chapter
    // and thumbnail image extraction — so they are pinned in full.
    #[case(
        Some(Video3DFormat::HalfSideBySide),
        "crop=iw/2:ih:0:0,scale=(iw*2):ih,setdar=dar=a,"
    )]
    #[case(Some(Video3DFormat::FullSideBySide), "crop=iw/2:ih:0:0,setdar=dar=a,")]
    // The corrected bracket: upstream writes `scale=(iw*2):ih)`, which ffmpeg's
    // expression parser rejects outright, so no image is produced at all.
    #[case(
        Some(Video3DFormat::HalfTopAndBottom),
        "crop=iw:ih/2:0:0,scale=(iw*2):ih,setdar=dar=a,"
    )]
    #[case(
        Some(Video3DFormat::FullTopAndBottom),
        "crop=iw:ih/2:0:0,setdar=dar=a,"
    )]
    // MVC is not frame-packed, so it falls to the same plain even-dimension
    // scaler a non-3D source gets.
    #[case(Some(Video3DFormat::Mvc), "scale=round(iw*sar/2)*2:round(ih/2)*2")]
    #[case(None, "scale=round(iw*sar/2)*2:round(ih/2)*2")]
    fn each_3d_layout_selects_its_own_scaler(
        #[case] threed_format: Option<Video3DFormat>,
        #[case] expected_prefix: &str,
    ) {
        let args = MediaEncoderImpl::<NoopTranscoder>::extract_image_arguments(
            "file:\"/media/movie.mkv\"",
            "",
            None,
            None,
            threed_format,
            None,
            true,
            "/tmp/out.jpg",
            0,
        );
        assert!(args.contains(expected_prefix), "got: {args}");
    }

    #[test]
    fn every_3d_scaler_has_balanced_brackets() {
        // The failure this guards is a single stray `)` — upstream carries one
        // in the half-top-and-bottom arm, and ffmpeg refuses the whole filter
        // graph over it rather than degrading. Any future re-sync from upstream
        // that reintroduces it fails here.
        for threed_format in [
            Some(Video3DFormat::HalfSideBySide),
            Some(Video3DFormat::FullSideBySide),
            Some(Video3DFormat::HalfTopAndBottom),
            Some(Video3DFormat::FullTopAndBottom),
            Some(Video3DFormat::Mvc),
            None,
        ] {
            let args = MediaEncoderImpl::<NoopTranscoder>::extract_image_arguments(
                "file:\"/media/movie.mkv\"",
                "",
                None,
                None,
                threed_format,
                None,
                true,
                "/tmp/out.jpg",
                0,
            );
            assert_eq!(
                args.matches('(').count(),
                args.matches(')').count(),
                "{threed_format:?} produced unbalanced brackets: {args}"
            );
        }
    }

    // An extraction temp directory the process cannot write fails every frame
    // for every video while ffmpeg itself is healthy. Catch it where the path
    // is known and say so, instead of reporting each video as frameless.
    #[cfg(unix)]
    #[tokio::test]
    async fn extract_video_image_names_an_unwritable_temp_dir() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let temp_dir = tmp.path().join("temp");
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::set_permissions(&temp_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let enc = MediaEncoderImpl::new(
            Arc::new(FrameWritingTranscoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: temp_dir.clone(),
                ..MediaEncoderConfig::default()
            },
        );
        let outcome = enc
            .extract_video_image(
                "/read-only/media/episode.mkv",
                "",
                &MediaSourceInfo::default(),
                &video_stream(),
                None,
                None,
            )
            .await;

        // Root ignores the mode bits; only assert when the probe is meaningful.
        if std::fs::File::create(temp_dir.join("probe")).is_err() {
            let err = outcome.expect_err("an unwritable temp dir must fail");
            let text = err.to_string();
            assert!(text.contains("not writable"), "got: {text}");
            assert!(
                text.contains("temp"),
                "the error names the path, got: {text}"
            );
        }

        std::fs::set_permissions(&temp_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
