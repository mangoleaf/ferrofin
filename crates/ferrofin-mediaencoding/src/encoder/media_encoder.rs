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
//! the hardware-transcoding roadmap (this unit gains its accelerated trickplay
//! path in that plan's phase 9); Blu-ray (`BdInfo`) has no plan yet and belongs
//! to disc-image playback rather than encoding. Every ffmpeg/ffprobe process
//! spawn sits behind the [`Transcoder`] seam so unit tests inject a fake.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::RwLock;

use crate::error::MediaEncodingError;
use async_trait::async_trait;
use ferrofin_model::drawing::{ImageFormat, ImageResolution};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{IsoType, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::{MediaInfo, MediaProtocol};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

use super::encoding_utils::{get_input_argument, quote};
use super::{FfmpegVersion, ProcessOutput, Transcoder};
use crate::probing::dtos::InternalMediaInfoResult;
use crate::probing::localization::{
    LocalizationManager as ProbeLocalization, PassthroughLocalization, TraitLocalization,
};
use crate::probing::probe_result_normalizer::ProbeResultNormalizer;

/// The number of 100-nanosecond ticks in one second (`TimeSpan.TicksPerSecond`).
#[cfg(test)]
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Jellyfin's default `FFmpeg:analyzeduration` (`ConfigurationOptions.cs:19`):
/// how long ffmpeg and ffprobe analyse an input to find its streams, when the
/// source names no duration of its own.
pub const DEFAULT_ANALYZE_DURATION: &str = "200M";

/// Jellyfin's default `FFmpeg:probesize` (`ConfigurationOptions.cs:18`): how
/// much of an input ffmpeg and ffprobe read to find its streams.
pub const DEFAULT_PROBE_SIZE: &str = "1G";

/// The `-analyzeduration` value upstream picks for an input
/// (`EncodingHelper.GetFfmpegAnalyzeDurationArg` and
/// `MediaEncoder.GetExtraArguments`): the source's own `AnalyzeDurationMs` in
/// microseconds when it is positive, else `configured`. Empty when neither is
/// set, which omits the flag.
#[must_use]
pub fn analyze_duration_value(source_ms: Option<i32>, configured: &str) -> String {
    match source_ms {
        Some(ms) if ms > 0 => (i64::from(ms) * 1000).to_string(),
        _ => configured.to_owned(),
    }
}

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
    pub temp_dir: ferrofin_util::directory_path::DirectoryPath,
    /// The probed ffmpeg version, which decides `-fps_mode` versus the
    /// deprecated `-vsync` (`GetVideoSyncOption`). `None` means "unprobed" and
    /// takes `-vsync`, which every supported build still understands.
    pub ffmpeg_version: Option<FfmpegVersion>,
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
    image_encoding_pool: Arc<tokio::sync::Semaphore>,
    chapter_image_resolution: Option<Arc<dyn Fn() -> ImageResolution + Send + Sync>>,
    config: MediaEncoderConfig,
    paths: RwLock<Paths>,
    normalizer: ProbeResultNormalizer<Box<dyn ProbeLocalization + Send + Sync>>,
}

impl<T: Transcoder> MediaEncoderImpl<T> {
    /// Shares the startup thumbnail/trickplay ceiling with the other extractor.
    /// The host resolves the dashboard setting and supplies a nonzero pool.
    #[must_use]
    pub fn with_image_encoding_pool(mut self, pool: Arc<tokio::sync::Semaphore>) -> Self {
        self.image_encoding_pool = pool;
        self
    }

    /// Reads the live dashboard resolution before each video image extraction.
    /// Audio cover extraction remains at source size, as in Jellyfin.
    #[must_use]
    pub fn with_chapter_image_resolution(
        mut self,
        read: impl Fn() -> ImageResolution + Send + Sync + 'static,
    ) -> Self {
        self.chapter_image_resolution = Some(Arc::new(read));
        self
    }

    /// Stamps probe output with the server's real localized labels and
    /// language display names, as Jellyfin's production normalizer does
    /// (`ProbeResultNormalizer` takes the live `ILocalizationManager`). Without
    /// it the encoder keeps [`PassthroughLocalization`].
    #[must_use]
    pub fn with_localization(
        mut self,
        localization: Arc<dyn ferrofin_traits::localization::LocalizationManager>,
    ) -> Self {
        self.normalizer = ProbeResultNormalizer::new(Box::new(TraitLocalization(localization)));
        self
    }

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
            image_encoding_pool: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            config,
            chapter_image_resolution: None,
            paths: RwLock::new(Paths {
                ffmpeg: ffmpeg_path.into(),
                ffprobe: ffprobe_path.into(),
            }),
            normalizer: ProbeResultNormalizer::new(Box::new(PassthroughLocalization)),
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

        let analyze_duration =
            analyze_duration_value(source.analyze_duration_ms, ffmpeg_analyze_duration);
        let mut extra_args = String::new();
        if !analyze_duration.is_empty() {
            extra_args = format!("-analyzeduration {analyze_duration}");
        }

        if !ffmpeg_probe_size.is_empty() {
            let _ = write!(extra_args, " -probesize {ffmpeg_probe_size}");
        }

        if let Some(user_agent) = source.required_http_headers.get("User-Agent") {
            let _ = write!(extra_args, " -user_agent {}", quote(user_agent));
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
    /// `GetMediaInfoInternal`'s format string).
    ///
    /// One `format!`, as upstream fills the line with one `string.Format`: the
    /// input path is inserted once and never re-scanned, so a file name
    /// holding `{threads}` reaches ffprobe unchanged.
    ///
    /// TODO(parity, open work item gl-347): upstream also appends
    /// `-show_frames -only_first_vframe` for a local, non-audio probe when
    /// ffprobe supports `only_first_vframe`, which is what sets
    /// `Hdr10PlusPresentFlag`.
    fn probe_arguments(&self, input_path: &str, extract_chapters: bool, extra: &str) -> String {
        let chapters = if extract_chapters {
            " -show_chapters"
        } else {
            ""
        };
        let threads = self.config.threads;
        format!(
            "{extra} -i {input_path} -threads {threads} -v warning -print_format json \
             -show_streams{chapters} -show_format"
        )
        .trim()
        .to_owned()
    }

    /// Builds the `ExtractImageInternal` argument line (software path only).
    ///
    /// Port of the core of `ExtractImageInternal`: the deinterlace + scale
    /// filter chain, optional thumbnail sampling, stream `-map`, and `-ss`
    /// offset.
    ///
    /// The HDR tonemap branch is tracked by the hardware-transcoding roadmap.
    /// The dashboard's image resolution applies after the video filter chain.
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
        ffmpeg_version: Option<FfmpegVersion>,
        threads: i32,
        image_resolution: ImageResolution,
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
        // `-fps_mode auto` (`-vsync -1` below ffmpeg 5.1): let ffmpeg decide,
        // which is what upstream passes here.
        let sync = crate::encoding_helper::helper::video_sync_option("-1", ffmpeg_version);
        let resolution = image_resolution_parameter(image_resolution);
        // The graph is quoted, unlike upstream's bare `-vf {3}`: the 3D
        // scalers contain spaces, so the split into argv would cut them into
        // stray ffmpeg arguments and 3D sources would get no images (a
        // Jellyfin bug, not ported).
        let mut args = format!(
            "-i {input_path}{map_arg} -threads {threads} -v error -vframes 1 -vf {vf}{resolution}{sync} -f image2 {output}",
            vf = quote(&vf),
            output = quote(output_path),
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

/// `GetImageResolutionParameter`: fixed output dimensions, including upscaling.
fn image_resolution_parameter(resolution: ImageResolution) -> &'static str {
    match resolution {
        ImageResolution::P144 => " -s 256x144",
        ImageResolution::P240 => " -s 426x240",
        ImageResolution::P360 => " -s 640x360",
        ImageResolution::P480 => " -s 854x480",
        ImageResolution::P720 => " -s 1280x720",
        ImageResolution::P1080 => " -s 1920x1080",
        ImageResolution::P1440 => " -s 2560x1440",
        ImageResolution::P2160 => " -s 3840x2160",
        ImageResolution::MatchSource | ImageResolution::Unrecognized(_) => "",
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
            .get_process_output(&probe, &args, false, None, &[])
            .await
            .map(ProcessOutput::into_output)
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
        // Match video extraction: media libraries may be mounted read-only.
        // The scanner copies the result into the item's data/metadata directory.
        let configured_temp = self.config.temp_dir.resolve();
        let temp_dir = if configured_temp.as_os_str().is_empty() {
            std::env::temp_dir()
        } else {
            configured_temp
        };
        ferrofin_util::file_helper::ensure_writable_dir(&temp_dir).map_err(|e| {
            MediaEncodingError::process(format!(
                "audio-image extraction temp directory `{}` is not writable: {e}",
                temp_dir.display()
            ))
        })?;
        let output_path = temp_dir
            .join(format!(
                "ferrofin-audio-{}.jpg",
                uuid::Uuid::new_v4().simple()
            ))
            .to_string_lossy()
            .into_owned();
        let args = Self::extract_image_arguments(
            &get_input_argument("file", path, MediaProtocol::File),
            "",
            None,
            image_stream_index,
            None,
            None,
            false,
            &output_path,
            self.config.ffmpeg_version,
            self.config.threads,
            ImageResolution::MatchSource,
        );
        let ffmpeg = self.encoder_path();
        let _permit = self
            .image_encoding_pool
            .acquire()
            .await
            .map_err(ServiceError::backend_source)?;
        let result = self
            .transcoder
            .get_process_output(&ffmpeg, &args, true, None, &[])
            .await;
        let output = match result {
            Ok(output) => output,
            Err(err) => {
                let _ = std::fs::remove_file(&output_path);
                return Err(MediaEncodingError::process(err).into());
            }
        };
        if !output.success
            || !std::fs::metadata(&output_path).is_ok_and(|m| m.is_file() && m.len() > 0)
        {
            let _ = std::fs::remove_file(&output_path);
            let tail: String = output
                .output
                .chars()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(MediaEncodingError::process(format!(
                "ffmpeg failed to extract audio image from `{path}`: {}",
                if tail.trim().is_empty() {
                    "no image produced or unsuccessful exit (no stderr)"
                } else {
                    tail.trim()
                }
            ))
            .into());
        }
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
        let configured_temp = self.config.temp_dir.resolve();
        let temp_dir = if configured_temp.as_os_str().is_empty() {
            std::env::temp_dir()
        } else {
            configured_temp
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
            self.config.ffmpeg_version,
            self.config.threads,
            self.chapter_image_resolution
                .as_ref()
                .map_or(ImageResolution::MatchSource, |read| read()),
        );
        let ffmpeg = self.encoder_path();
        let _permit = self
            .image_encoding_pool
            .acquire()
            .await
            .map_err(ServiceError::backend_source)?;
        let stderr = self
            .transcoder
            .get_process_output(&ffmpeg, &args, true, None, &[])
            .await
            .map(ProcessOutput::into_output)
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

    async fn extract_image_stream(
        &self,
        input_file: &str,
        container: &str,
        media_source: &MediaSourceInfo,
        stream_index: i32,
        format: ImageFormat,
    ) -> Result<String, ServiceError> {
        // Same temp-directory rules as the frame extraction above.
        let configured_temp = self.config.temp_dir.resolve();
        let temp_dir = if configured_temp.as_os_str().is_empty() {
            std::env::temp_dir()
        } else {
            configured_temp
        };
        ferrofin_util::file_helper::ensure_writable_dir(&temp_dir).map_err(|e| {
            MediaEncodingError::process(format!(
                "image extraction temp directory `{}` is not writable: {e}",
                temp_dir.display()
            ))
        })?;
        let input_path = self.get_input_argument(input_file, media_source);
        let ffmpeg = self.encoder_path();
        let _permit = self
            .image_encoding_pool
            .acquire()
            .await
            .map_err(ServiceError::backend_source)?;
        // `ExtractImage` tries the I-frame path first and, when that leaves
        // nothing, the standard one.
        let mut last_error = String::new();
        for use_iframe in [true, false] {
            let output_path = temp_dir
                .join(format!(
                    "ferrofin-image-{}{}",
                    uuid::Uuid::new_v4().simple(),
                    format.extension()
                ))
                .to_string_lossy()
                .into_owned();
            let args = Self::extract_image_arguments(
                &input_path,
                container,
                None,
                Some(stream_index),
                None,
                None,
                use_iframe,
                &output_path,
                self.config.ffmpeg_version,
                self.config.threads,
                self.chapter_image_resolution
                    .as_ref()
                    .map_or(ImageResolution::MatchSource, |read| read()),
            );
            let output = self
                .transcoder
                .get_process_output(&ffmpeg, &args, true, None, &[])
                .await
                .map(ProcessOutput::into_output)
                .map_err(MediaEncodingError::process)?;
            if std::fs::metadata(&output_path).is_ok_and(|m| m.is_file() && m.len() > 0) {
                return Ok(output_path);
            }
            let _ = std::fs::remove_file(&output_path);
            last_error = output
                .chars()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }
        Err(MediaEncodingError::process(format!(
            "ffmpeg image extraction failed for `{input_file}` stream {stream_index}: {}",
            if last_error.trim().is_empty() {
                "(no stderr)"
            } else {
                last_error.trim()
            }
        ))
        .into())
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
    use super::{FfmpegVersion, ProcessOutput};
    use ferrofin_model::entities::Video3DFormat;
    use rstest::rstest;

    use std::sync::Arc;

    use ferrofin_model::drawing::{ImageFormat, ImageResolution};
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
            _env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            Ok(ProcessOutput {
                output: String::new(),
                success: true,
            })
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

    /// Upstream interpolates the tuner User-Agent raw; a value that closes the
    /// quote must stay the one `-user_agent` argument instead.
    #[test]
    fn a_hostile_user_agent_stays_one_argument() {
        let user_agent = r#"x\" -y -i /etc/passwd ""#;
        let request = MediaInfoRequest {
            media_source: http_source_with_user_agent(user_agent),
            extract_chapters: false,
            media_is_audio: false,
        };
        let extra = encoder().get_extra_arguments(&request);
        let tokens = crate::encoder::split_args(&extra).unwrap();
        let at = tokens.iter().position(|a| a == "-user_agent").unwrap();
        assert_eq!(tokens[at + 1], user_agent);
        assert!(!tokens.iter().any(|a| a == "-y"), "{tokens:?}");
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

    #[rstest::rstest]
    #[case(Some(3000), "200M", "3000000")]
    #[case(Some(0), "200M", "200M")]
    #[case(Some(-1), "200M", "200M")]
    #[case(None, "200M", "200M")]
    #[case(None, "", "")]
    fn analyze_duration_prefers_a_positive_source_value(
        #[case] source_ms: Option<i32>,
        #[case] configured: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            super::analyze_duration_value(source_ms, configured),
            expected
        );
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

    /// A [`Transcoder`] fake that "extracts a frame": it splits the arguments
    /// as the real spawn does, takes the output path off the tail (the shape
    /// `extract_image_arguments` emits) and writes a stub file there, like a
    /// successful ffmpeg run.
    struct FrameWritingTranscoder;

    #[async_trait]
    impl Transcoder for FrameWritingTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
            _env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            let argv = crate::encoder::split_args(arguments)?;
            let out = argv.last().expect("the output path is the last argument");
            std::fs::write(out, b"jpg").expect("write frame");
            Ok(ProcessOutput {
                output: String::new(),
                success: true,
            })
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    /// A transcoder that records its arguments and writes nothing until its
    /// `succeed_on`th call (zero-based), as ffmpeg does when a picture cannot
    /// be extracted the quick way.
    struct FlakyTranscoder {
        calls: std::sync::Mutex<Vec<String>>,
        succeed_on: usize,
    }

    #[async_trait]
    impl Transcoder for FlakyTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
            _env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            let attempt = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(arguments.to_owned());
                calls.len() - 1
            };
            if attempt >= self.succeed_on {
                let argv = crate::encoder::split_args(arguments)?;
                let out = argv.last().expect("output path");
                std::fs::write(out, b"png").expect("write picture");
            }
            Ok(ProcessOutput {
                output: "ffmpeg log tail".to_owned(),
                success: true,
            })
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    /// `ExtractVideoImage(…, imageStream, imageStreamIndex, targetFormat, …)`:
    /// the picture stream is mapped by index, written under the requested
    /// extension in the temp dir, and the standard path is tried when the
    /// I-frame one leaves nothing.
    #[tokio::test]
    async fn extract_image_stream_maps_the_stream_and_retries_without_the_iframe_path() {
        let tmp = tempfile::tempdir().unwrap();
        let transcoder = Arc::new(FlakyTranscoder {
            calls: std::sync::Mutex::new(Vec::new()),
            succeed_on: 1,
        });
        let enc = MediaEncoderImpl::new(
            Arc::clone(&transcoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: tmp.path().to_path_buf().into(),
                ..MediaEncoderConfig::default()
            },
        );
        let out = enc
            .extract_image_stream(
                "/media/movie.mkv",
                "matroska",
                &MediaSourceInfo::default(),
                3,
                ImageFormat::Png,
            )
            .await
            .expect("second attempt succeeds");
        assert_eq!(
            std::path::Path::new(&out)
                .extension()
                .and_then(|e| e.to_str()),
            Some("png"),
            "{out}"
        );
        assert!(std::path::Path::new(&out).starts_with(tmp.path()));
        let calls = transcoder.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert!(
            calls.iter().all(|call| call.contains(" -map 0:3")),
            "{calls:?}"
        );
        // Nothing but the extracted picture is left in the temp dir.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn extract_image_stream_reports_ffmpeg_output_when_nothing_is_produced() {
        let tmp = tempfile::tempdir().unwrap();
        let enc = MediaEncoderImpl::new(
            Arc::new(FlakyTranscoder {
                calls: std::sync::Mutex::new(Vec::new()),
                succeed_on: usize::MAX,
            }),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: tmp.path().to_path_buf().into(),
                ..MediaEncoderConfig::default()
            },
        );
        let error = enc
            .extract_image_stream(
                "/media/movie.mkv",
                "",
                &MediaSourceInfo::default(),
                9,
                ImageFormat::Jpg,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("stream 9") && error.contains("ffmpeg log tail"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn extract_audio_image_uses_unique_files_in_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let temp_dir = tmp.path().join("cache/temp");
        let enc = MediaEncoderImpl::new(
            Arc::new(FrameWritingTranscoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: temp_dir.clone().into(),
                ..MediaEncoderConfig::default()
            },
        );
        let first = enc
            .extract_audio_image("/read-only/music/track.flac", Some(1))
            .await
            .unwrap();
        let second = enc
            .extract_audio_image("/read-only/music/track.flac", Some(1))
            .await
            .unwrap();
        assert_ne!(first, second);
        for out in [first, second] {
            assert!(std::path::Path::new(&out).starts_with(&temp_dir));
            assert_eq!(std::fs::read(out).unwrap(), b"jpg");
        }
    }

    struct FailedAudioTranscoder;

    #[async_trait]
    impl Transcoder for FailedAudioTranscoder {
        async fn get_process_output(
            &self,
            path: &str,
            arguments: &str,
            read_stderr: bool,
            test_key: Option<&str>,
            env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            // FFmpeg can leave a partial file even when it fails.
            FrameWritingTranscoder
                .get_process_output(path, arguments, read_stderr, test_key, env)
                .await?;
            Ok(ProcessOutput {
                success: false,
                output: "test encoder failure".to_owned(),
            })
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn extract_audio_image_reports_ffmpeg_failure_and_removes_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let enc = MediaEncoderImpl::new(
            Arc::new(FailedAudioTranscoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: tmp.path().to_path_buf().into(),
                ..MediaEncoderConfig::default()
            },
        );
        let err = enc
            .extract_audio_image("/read-only/music/track.flac", Some(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("test encoder failure"));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn extract_audio_image_rejects_missing_output() {
        let tmp = tempfile::tempdir().unwrap();
        let enc = MediaEncoderImpl::new(
            Arc::new(NoopTranscoder),
            "/usr/bin/ffmpeg".to_owned(),
            "/usr/bin/ffprobe".to_owned(),
            MediaEncoderConfig {
                temp_dir: tmp.path().to_path_buf().into(),
                ..MediaEncoderConfig::default()
            },
        );
        let err = enc
            .extract_audio_image("/read-only/music/track.flac", Some(1))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("ffmpeg failed to extract audio image")
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
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
                temp_dir: tmp.path().to_path_buf().into(),
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
                temp_dir: tmp.path().to_path_buf().into(),
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
            None,
            0,
            ImageResolution::MatchSource,
        );
        assert!(args.contains("-v error"), "got: {args}");
        assert!(!args.contains("-v quiet"), "got: {args}");
    }

    // Upstream passes `GetVideoSyncOption("-1")` here -- "let ffmpeg decide" --
    // between the filter and the muxer. It is a single frame either way, but
    // the option is version-gated and lands in the middle of the line, so the
    // position is worth pinning.
    #[rstest]
    #[case(None, " -vsync -1 -f image2")]
    #[case(Some(FfmpegVersion::new(5, 0)), " -vsync -1 -f image2")]
    #[case(Some(FfmpegVersion::new(5, 1)), " -fps_mode auto -f image2")]
    #[case(Some(FfmpegVersion::with_build(7, 0, 1)), " -fps_mode auto -f image2")]
    fn extraction_lets_ffmpeg_decide_the_frame_rate_mode(
        #[case] version: Option<FfmpegVersion>,
        #[case] expected: &str,
    ) {
        let args = MediaEncoderImpl::<NoopTranscoder>::extract_image_arguments(
            "file:\"/media/episode.mkv\"",
            "",
            None,
            None,
            None,
            None,
            true,
            "/tmp/out.jpg",
            version,
            0,
            ImageResolution::MatchSource,
        );
        assert!(args.contains(expected), "got: {args}");
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
            None,
            0,
            ImageResolution::MatchSource,
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
                None,
                0,
                ImageResolution::MatchSource,
            );
            assert_eq!(
                args.matches('(').count(),
                args.matches(')').count(),
                "{threed_format:?} produced unbalanced brackets: {args}"
            );
        }
    }

    // The 3D graphs carry spaces and ffmpeg's `\,` escape, and the argument
    // string is split into argv by `split_args`. Upstream's bare `-vf {3}`
    // splits such a graph at every space; quoted, ffmpeg gets it whole.
    #[test]
    fn each_3d_scaler_reaches_ffmpeg_as_one_filter_argument() {
        for threed_format in [
            Video3DFormat::HalfSideBySide,
            Video3DFormat::FullSideBySide,
            Video3DFormat::HalfTopAndBottom,
            Video3DFormat::FullTopAndBottom,
        ] {
            let args = MediaEncoderImpl::<NoopTranscoder>::extract_image_arguments(
                "file:\"/media/movie.mkv\"",
                "",
                None,
                None,
                Some(threed_format),
                None,
                true,
                "/tmp/out.jpg",
                None,
                0,
                ImageResolution::MatchSource,
            );
            let tokens = crate::encoder::split_args(&args).unwrap();
            let vf = tokens.iter().position(|a| a == "-vf").unwrap() + 1;
            let graph = &tokens[vf];
            assert!(
                graph.starts_with("crop=iw")
                    && graph.contains(",crop=min(iw\\,ih*dar):min(ih\\,iw/dar):")
                    && graph.ends_with(":(ih - min (ih\\,ih/sar))/2,setsar=sar=1,thumbnail=n=24"),
                "{threed_format:?}: {tokens:?}"
            );
            assert_eq!(tokens[vf + 1], "-vsync", "{threed_format:?}: {tokens:?}");
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
                temp_dir: temp_dir.clone().into(),
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

    struct GatedFrames {
        entered: tokio::sync::Semaphore,
        release: tokio::sync::Semaphore,
    }

    #[async_trait]
    impl Transcoder for GatedFrames {
        async fn get_process_output(
            &self,
            _: &str,
            arguments: &str,
            _: bool,
            _: Option<&str>,
            _: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            let args = crate::encoder::split_args(arguments)?;
            let path = args.last().unwrap().replace("%08d", "00000001");
            std::fs::write(path, b"jpg").unwrap();
            Ok(ProcessOutput {
                output: String::new(),
                success: true,
            })
        }
        async fn get_process_exit_code(&self, _: &str, _: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn audio_video_and_trickplay_share_thumbnail_slots_and_release_on_cancel() {
        use ferrofin_model::entities_media::MediaStream;
        use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayFrameExtractor as _};
        let tmp = tempfile::tempdir().unwrap();
        let pool = Arc::new(tokio::sync::Semaphore::new(2));
        let process = Arc::new(GatedFrames {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let encoder = Arc::new(
            MediaEncoderImpl::new(
                Arc::clone(&process),
                "ffmpeg",
                "ffprobe",
                MediaEncoderConfig {
                    temp_dir: tmp.path().into(),
                    ..Default::default()
                },
            )
            .with_image_encoding_pool(Arc::clone(&pool)),
        );
        let audio = {
            let encoder = Arc::clone(&encoder);
            tokio::spawn(async move { encoder.extract_audio_image("/audio.flac", None).await })
        };
        let video = {
            let encoder = Arc::clone(&encoder);
            tokio::spawn(async move {
                encoder
                    .extract_video_image(
                        "/movie.mkv",
                        "mkv",
                        &MediaSourceInfo::default(),
                        &MediaStream::default(),
                        None,
                        None,
                    )
                    .await
            })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            process.entered.acquire_many(2),
        )
        .await
        .unwrap()
        .unwrap()
        .forget();
        let extractor =
            crate::encoder::TrickplayFrameExtractorImpl::new(Arc::clone(&process), "ffmpeg", None)
                .with_image_encoding_pool(Arc::clone(&pool));
        let output_dir = tmp.path().join("trickplay").to_string_lossy().into_owned();
        let trickplay = tokio::spawn(async move {
            extractor
                .extract_trickplay_frames(&TrickplayExtraction {
                    input_path: "/movie.mkv",
                    video_stream: None,
                    interval_ms: 10_000,
                    max_width: 320,
                    qscale: 4,
                    threads: 1,
                    output_dir: &output_dir,
                    allow_hw_accel: false,
                    enable_hw_encoding: false,
                    keyframe_only: false,
                })
                .await
        });
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                process.entered.acquire()
            )
            .await
            .is_err()
        );
        audio.abort();
        assert!(audio.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(3), process.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(pool.available_permits(), 0);
        process.release.add_permits(2);
        video.await.unwrap().unwrap();
        assert_eq!(trickplay.await.unwrap().unwrap().len(), 1);
        assert_eq!(pool.available_permits(), 2);
    }
    struct RecordingFrames(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl Transcoder for RecordingFrames {
        async fn get_process_output(
            &self,
            path: &str,
            arguments: &str,
            read_stderr: bool,
            test_key: Option<&str>,
            env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            self.0.lock().unwrap().push(arguments.to_owned());
            FrameWritingTranscoder
                .get_process_output(path, arguments, read_stderr, test_key, env)
                .await
        }
        async fn get_process_exit_code(&self, _: &str, _: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn live_chapter_resolution_changes_video_arguments_but_audio_keeps_source_size() {
        let temp = tempfile::tempdir().unwrap();
        let transcoder = Arc::new(RecordingFrames(std::sync::Mutex::new(Vec::new())));
        let resolution = Arc::new(std::sync::Mutex::new(ImageResolution::P144));
        let reader = Arc::clone(&resolution);
        let encoder = MediaEncoderImpl::new(
            Arc::clone(&transcoder),
            "ffmpeg",
            "ffprobe",
            MediaEncoderConfig {
                temp_dir: temp.path().to_path_buf().into(),
                ..Default::default()
            },
        )
        .with_chapter_image_resolution(move || *reader.lock().unwrap());
        for (setting, expected) in [
            (ImageResolution::P144, Some("256x144")),
            (ImageResolution::P480, Some("854x480")),
            (ImageResolution::MatchSource, None),
            (ImageResolution::Unrecognized(99), None),
        ] {
            *resolution.lock().unwrap() = setting;
            let frame = encoder
                .extract_video_image(
                    "movie.mkv",
                    "mkv",
                    &MediaSourceInfo::default(),
                    &video_stream(),
                    None,
                    Some(0),
                )
                .await
                .unwrap();
            std::fs::remove_file(frame).unwrap();
            let arguments = transcoder.0.lock().unwrap().last().unwrap().clone();
            if let Some(expected) = expected {
                assert!(
                    arguments.contains(&format!(" -s {expected} ")),
                    "{arguments}"
                );
            } else {
                assert!(!arguments.contains(" -s "), "{arguments}");
            }
        }
        *resolution.lock().unwrap() = ImageResolution::P2160;
        let audio = encoder
            .extract_audio_image("track.flac", Some(1))
            .await
            .unwrap();
        std::fs::remove_file(audio).unwrap();
        assert!(
            !transcoder
                .0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains(" -s ")
        );
    }

    /// Cover art is extracted unprompted during a music scan, so a track named
    /// to close the quote must not inject options: the whole name stays the
    /// one `-i` value, as upstream's `GetInputArgument` keeps it.
    #[tokio::test]
    async fn audio_image_keeps_a_hostile_path_as_one_input() {
        let temp = tempfile::tempdir().unwrap();
        let transcoder = Arc::new(RecordingFrames(std::sync::Mutex::new(Vec::new())));
        let encoder = MediaEncoderImpl::new(
            Arc::clone(&transcoder),
            "ffmpeg",
            "ffprobe",
            MediaEncoderConfig {
                temp_dir: temp.path().to_path_buf().into(),
                ..Default::default()
            },
        );
        let path = r#"/m/a" -y -f mjpeg /data/ferrofin.db ".flac"#;
        let image = encoder.extract_audio_image(path, Some(1)).await.unwrap();
        std::fs::remove_file(image).unwrap();
        let arguments = transcoder.0.lock().unwrap().last().unwrap().clone();
        let tokens = crate::encoder::split_args(&arguments).unwrap();
        assert_eq!(tokens[..2], ["-i".to_owned(), format!("file:{path}")]);
        assert!(!tokens.iter().any(|a| a == "-y"), "{tokens:?}");
    }

    #[test]
    fn every_dashboard_resolution_uses_its_pinned_output_dimensions() {
        use super::image_resolution_parameter;

        for (resolution, dimensions) in [
            (ImageResolution::P144, "256x144"),
            (ImageResolution::P240, "426x240"),
            (ImageResolution::P360, "640x360"),
            (ImageResolution::P480, "854x480"),
            (ImageResolution::P720, "1280x720"),
            (ImageResolution::P1080, "1920x1080"),
            (ImageResolution::P1440, "2560x1440"),
            (ImageResolution::P2160, "3840x2160"),
        ] {
            let arguments = MediaEncoderImpl::<NoopTranscoder>::extract_image_arguments(
                "movie.mkv",
                "mkv",
                Some(&video_stream()),
                None,
                None,
                Some(150_000_000),
                true,
                "frame.jpg",
                None,
                0,
                resolution,
            );
            assert!(
                arguments.contains(&format!(" -s {dimensions} ")),
                "{resolution:?}: {arguments}"
            );
            assert!(arguments.contains("-ss 00:00:15.000"));
        }
        assert_eq!(image_resolution_parameter(ImageResolution::MatchSource), "");
        assert_eq!(
            image_resolution_parameter(ImageResolution::Unrecognized(-1)),
            ""
        );
    }

    /// A [`Transcoder`] fake whose ffprobe run reports two audio tracks.
    struct TwoAudioProbeTranscoder;

    #[async_trait]
    impl Transcoder for TwoAudioProbeTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            _arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
            _env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            let output = r#"{"streams":[
                {"index":0,"codec_type":"audio","codec_name":"aac",
                 "disposition":{"original":1},"tags":{"language":"eng"}},
                {"index":1,"codec_type":"audio","codec_name":"aac",
                 "tags":{"language":"xyz"}}
            ],"format":{"format_name":"matroska"}}"#;
            Ok(ProcessOutput {
                output: output.to_owned(),
                success: true,
            })
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    /// The server localization seam, knowing only `eng`.
    struct EnglishOnly;

    impl ferrofin_traits::localization::LocalizationManager for EnglishOnly {
        fn get_cultures(&self) -> Vec<ferrofin_model::globalization::CultureDto> {
            Vec::new()
        }
        fn get_countries(&self) -> Vec<ferrofin_model::globalization::CountryInfo> {
            Vec::new()
        }
        fn get_parental_ratings(&self) -> Vec<ferrofin_model::entities_media::ParentalRating> {
            Vec::new()
        }
        fn get_localization_options(
            &self,
        ) -> Vec<ferrofin_model::globalization::LocalizationOption> {
            Vec::new()
        }
        fn get_localized_string(&self, phrase: &str) -> String {
            format!("localized {phrase}")
        }
        fn get_server_localized_string(&self, phrase: &str) -> String {
            phrase.to_owned()
        }
        fn get_localized_string_for(&self, phrase: &str, _culture: &str) -> String {
            phrase.to_owned()
        }
        fn find_language_info(
            &self,
            _language: &str,
        ) -> Option<ferrofin_model::globalization::CultureDto> {
            None
        }
        fn get_language_display_name(&self, language: &str) -> Option<String> {
            (language == "eng").then(|| "English".to_owned())
        }
        fn get_rating_score(
            &self,
            _rating: &str,
            _country_code: Option<&str>,
        ) -> Option<ferrofin_model::entities_media::ParentalRatingScore> {
            None
        }
    }

    // Jellyfin's production `ProbeResultNormalizer` holds the live
    // `ILocalizationManager` (ProbeResultNormalizer.cs:729-738): an unknown
    // language display name stays null, and every audio stream gets
    // `LocalizedOriginal`.
    #[tokio::test]
    async fn with_localization_stamps_real_labels_on_probe_output() {
        let enc = MediaEncoderImpl::new(
            Arc::new(TwoAudioProbeTranscoder),
            "/usr/bin/ffmpeg",
            "/usr/bin/ffprobe",
            MediaEncoderConfig::default(),
        )
        .with_localization(Arc::new(EnglishOnly));
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some("/media/film.mkv".to_owned()),
                ..MediaSourceInfo::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        let source = enc.get_media_info(&request).await.expect("probe parses");
        let streams = &source.media_streams;
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].localized_language.as_deref(), Some("English"));
        assert_eq!(
            streams[0].localized_original.as_deref(),
            Some("localized Original")
        );
        assert!(streams[0].is_original);
        assert_eq!(streams[1].localized_language, None);
        assert_eq!(
            streams[1].localized_original.as_deref(),
            Some("localized Original")
        );
        assert!(!streams[1].is_original);
    }

    /// A [`Transcoder`] fake that records each ffprobe argument line and
    /// answers with an empty probe.
    struct RecordingProbe(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl Transcoder for RecordingProbe {
        async fn get_process_output(
            &self,
            _path: &str,
            arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
            _env: &[(String, String)],
        ) -> Result<ProcessOutput, String> {
            self.0.lock().unwrap().push(arguments.to_owned());
            Ok(ProcessOutput {
                output: r#"{"streams":[],"format":{}}"#.to_owned(),
                success: true,
            })
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    // Upstream fills the probe line with one `string.Format`, so a file name
    // is never re-scanned for the template's own placeholders. A library file
    // whose name holds `{threads}` or `{input}` must reach ffprobe unchanged,
    // or it is probed under another name and silently gets no streams.
    #[rstest]
    #[case::without_chapters(false)]
    #[case::with_chapters(true)]
    #[tokio::test]
    async fn a_path_holding_template_placeholders_reaches_ffprobe_unchanged(
        #[case] extract_chapters: bool,
    ) {
        let transcoder = Arc::new(RecordingProbe(std::sync::Mutex::new(Vec::new())));
        let enc = MediaEncoderImpl::new(
            Arc::clone(&transcoder),
            "/usr/bin/ffmpeg",
            "/usr/bin/ffprobe",
            MediaEncoderConfig {
                threads: 3,
                ..MediaEncoderConfig::default()
            },
        );
        let path = "/media/{threads} {input} Live.mkv";
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some(path.to_owned()),
                ..MediaSourceInfo::default()
            },
            extract_chapters,
            media_is_audio: false,
        };
        enc.get_media_info(&request).await.expect("probe parses");

        let arguments = transcoder.0.lock().unwrap().last().unwrap().clone();
        let tokens = crate::encoder::split_args(&arguments).unwrap();
        let input = tokens.iter().position(|a| a == "-i").unwrap();
        assert_eq!(tokens[input + 1], format!("file:{path}"), "{arguments}");
        let threads = tokens.iter().position(|a| a == "-threads").unwrap();
        assert_eq!(tokens[threads + 1], "3", "{arguments}");
        assert_eq!(
            tokens.iter().any(|a| a == "-show_chapters"),
            extract_chapters,
            "{arguments}"
        );
    }
}
