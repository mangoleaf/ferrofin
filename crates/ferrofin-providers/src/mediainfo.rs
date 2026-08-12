//! The ffprobe-backed media-info provider — port of the object-safe subset of
//! `MediaBrowser.Providers.MediaInfo.FFProbeVideoInfo` (and the entry-point role
//! of `ProbeProvider`).
//!
//! Scope note (First-Light): the full C# `FFProbeVideoInfo` couples the probe to
//! ~13 collaborators (media-source / subtitle / chapter / attachment / stream
//! repositories, the Blu-ray/DVD examiners, the localization manager, and the
//! library store). None of those are ported in this wave, so this port keeps the
//! two pieces that are pure and testable and that only need the ffprobe seam:
//!
//! - [`FFProbeVideoInfo::get_media_info`] — builds a [`MediaInfoRequest`] from a
//!   [`VideoProbeInput`] and drives it through a borrowed
//!   `&dyn ferrofin_traits::media_encoding::MediaEncoder` (the ffprobe seam), so
//!   the un-mockable subprocess I/O stays behind the trait and out of these
//!   tests.
//! - [`FFProbeVideoInfo::fetch`] — the subset of `Fetch` that applies a probe
//!   result to the item (container / total bitrate / runtime / size, plus the
//!   video-stream-derived width/height/`HasSubtitles`) and re-indexes streams.
//! - [`FFProbeVideoInfo::create_dummy_chapters`] — the dummy-chapter generator,
//!   transliterated verbatim (it is the parity oracle for this unit — see the
//!   xUnit `FFProbeVideoInfoTests`).
//!
//! The Blu-ray/DVD `.vob`/`.m2ts` disc paths, the embedded-info / people
//! extraction, external subtitle/audio download, and the repository writes are
//! deferred to the Wave 6 implementation.

use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{IsoType, MediaStreamType, Video3DFormat, VideoType};
use ferrofin_model::entities_media::{ChapterInfo, MediaStream};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

/// Ticks per second (100 ns units), matching .NET `TimeSpan.TicksPerSecond`.
///
/// Port of the implicit `TimeSpan.FromSeconds` conversion the C# uses when it
/// turns `DummyChapterDuration` (seconds) into ticks.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The upper runtime bound `CreateDummyChapters` treats as valid, in ticks
/// (`TimeSpan.FromHours(12).Ticks`). Runtimes above this are assumed corrupt.
///
/// Port of the literal `TimeSpan.FromHours(12).Ticks` guard.
const MAX_VALID_RUNTIME_TICKS: i64 = 12 * 60 * 60 * TICKS_PER_SECOND;

/// The item subset [`FFProbeVideoInfo`] reads and writes — port of the fields of
/// `MediaBrowser.Controller.Entities.Video` the ported `Fetch`/`GetMediaInfo`
/// touch.
///
/// The full C# `Video` is a large library-item aggregate; this carries only the
/// probe-relevant surface so the provider stays decoupled from the (not-yet
/// ported) item store.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoProbeInput {
    /// The item name (`Name`) — used only for the invalid-runtime error message.
    pub name: Option<String>,
    /// The media file path (`Path`).
    pub path: Option<String>,
    /// The path protocol (`PathProtocol`); defaults to [`MediaProtocol::File`].
    pub path_protocol: Option<MediaProtocol>,
    /// The disc video type (`VideoType`), if any.
    pub video_type: Option<VideoType>,
    /// The ISO type (`IsoType`), if any.
    pub iso_type: Option<IsoType>,
    /// The 3D format (`Video3DFormat`), if any.
    pub video3d_format: Option<Video3DFormat>,
    /// The total runtime, in ticks (`RunTimeTicks`).
    pub run_time_ticks: Option<i64>,
    /// The container (`Container`).
    pub container: Option<String>,
    /// The total bitrate (`TotalBitrate`).
    pub total_bitrate: Option<i32>,
    /// The size in bytes (`Size`).
    pub size: Option<i64>,
    /// The video width (`Width`).
    pub width: i32,
    /// The video height (`Height`).
    pub height: i32,
    /// The default video-stream index (`DefaultVideoStreamIndex`).
    pub default_video_stream_index: Option<i32>,
    /// Whether the item has subtitle streams (`HasSubtitles`).
    pub has_subtitles: bool,
}

/// The ffprobe media-info provider (object-safe subset of `FFProbeVideoInfo`).
///
/// Borrows the ffprobe seam (`&dyn MediaEncoder`) rather than owning it, matching
/// the "any real process/network I/O behind a small trait" seam rule: unit tests
/// pass a fake encoder and the subprocess never runs.
pub struct FFProbeVideoInfo<'a> {
    encoder: &'a dyn MediaEncoder,
    dummy_chapter_duration_seconds: i32,
}

impl<'a> FFProbeVideoInfo<'a> {
    /// Creates a provider over the given `encoder` seam.
    ///
    /// `dummy_chapter_duration_seconds` is the `ServerConfiguration
    /// .DummyChapterDuration` value (in seconds) [`Self::create_dummy_chapters`]
    /// reads; a value of `0` disables dummy-chapter generation upstream.
    #[must_use]
    pub fn new(encoder: &'a dyn MediaEncoder, dummy_chapter_duration_seconds: i32) -> Self {
        Self {
            encoder,
            dummy_chapter_duration_seconds,
        }
    }

    /// Probes `item` through the ffprobe seam, returning its
    /// container/stream information.
    ///
    /// Port of the private `GetMediaInfo(Video, …)`: it builds the
    /// [`MediaInfoRequest`] (`ExtractChapters = true`) from the item path and
    /// protocol and defers the actual probe to the encoder.
    ///
    /// # Errors
    ///
    /// Propagates any [`ServiceError`] the encoder raises while probing.
    pub async fn get_media_info(
        &self,
        item: &VideoProbeInput,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let protocol = item.path_protocol.unwrap_or(MediaProtocol::File);

        let media_source = MediaSourceInfo {
            path: item.path.clone(),
            protocol,
            video_type: item.video_type,
            iso_type: item.iso_type,
            ..MediaSourceInfo::default()
        };

        let request = MediaInfoRequest {
            media_source,
            extract_chapters: true,
            media_is_audio: false,
        };

        self.encoder.get_media_info(&request).await
    }

    /// Applies a probe result to `video`, returning the fully-indexed stream
    /// list.
    ///
    /// Port of the object-safe subset of `Fetch`: it copies the container /
    /// total-bitrate / runtime (and, for disc sources, size) onto the item,
    /// re-indexes the streams `0..n`, then derives width / height /
    /// `DefaultVideoStreamIndex` / `HasSubtitles` from the video stream. The
    /// external-audio/subtitle download, embedded-info/people extraction,
    /// Blu-ray fixups, and repository writes are deferred.
    #[must_use]
    pub fn fetch(
        &self,
        video: &mut VideoProbeInput,
        media_info: Option<&MediaSourceInfo>,
    ) -> Vec<MediaStream> {
        let mut media_streams: Vec<MediaStream> = Vec::new();

        if let Some(info) = media_info {
            media_streams.extend(info.media_streams.iter().cloned());

            video.total_bitrate = info.bitrate;
            video.run_time_ticks = info.run_time_ticks;
            video.container.clone_from(&info.container);

            if matches!(video.video_type, Some(VideoType::BluRay | VideoType::Dvd)) {
                video.size = info.size;
            }
        }

        for (i, stream) in media_streams.iter_mut().enumerate() {
            stream.index = i32::try_from(i).unwrap_or(i32::MAX);
        }

        let video_stream = media_streams
            .iter()
            .find(|s| s.stream_type == MediaStreamType::Video);

        video.height = video_stream.and_then(|s| s.height).unwrap_or(0);
        video.width = video_stream.and_then(|s| s.width).unwrap_or(0);
        video.default_video_stream_index = video_stream.map(|s| s.index);
        video.has_subtitles = media_streams
            .iter()
            .any(|s| s.stream_type == MediaStreamType::Subtitle);

        media_streams
    }

    /// Generates evenly-spaced placeholder chapters for `video`.
    ///
    /// Port of `FFProbeVideoInfo.CreateDummyChapters` (transliterated verbatim —
    /// the xUnit `FFProbeVideoInfoTests` oracle). Only files with a runtime in
    /// `0..=12h` are processed; a runtime outside that range is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::invalid_input`] when the runtime is negative or
    /// exceeds 12 hours (the corrupt-file guard); mirrors the C#
    /// `ArgumentException`.
    pub fn create_dummy_chapters(
        &self,
        video: &VideoProbeInput,
    ) -> Result<Vec<ChapterInfo>, ServiceError> {
        let runtime = video.run_time_ticks.unwrap_or_default();

        // Only process files with a runtime greater than 0 and less than 12h.
        // The latter are likely corrupted.
        if !(0..=MAX_VALID_RUNTIME_TICKS).contains(&runtime) {
            // C# formats `TimeSpan.FromTicks(runtime).TotalMinutes`.
            let total_minutes =
                f64::from(i32::try_from(runtime / TICKS_PER_SECOND).unwrap_or(0)) / 60.0;
            return Err(ServiceError::invalid_input(format!(
                "{} has an invalid runtime of {total_minutes} minutes",
                video.name.as_deref().unwrap_or_default(),
            )));
        }

        let dummy_chapter_duration =
            i64::from(self.dummy_chapter_duration_seconds) * TICKS_PER_SECOND;

        if runtime <= 0 {
            return Ok(Vec::new());
        }

        // `runtime` is bounded by 12h of ticks and `dummy_chapter_duration >= 1s`
        // of ticks, so the quotient always fits in an `i32` (as the C# cast asserts).
        let chapter_count = std::cmp::max(1, runtime / dummy_chapter_duration);
        let mut chapters = Vec::with_capacity(usize::try_from(chapter_count).unwrap_or(0));

        let mut current_chapter_ticks: i64 = 0;
        for _ in 0..chapter_count {
            chapters.push(ChapterInfo {
                start_position_ticks: current_chapter_ticks,
                ..ChapterInfo::default()
            });

            current_chapter_ticks += dummy_chapter_duration;
        }

        Ok(chapters)
    }
}

#[cfg(test)]
mod tests {
    use super::{FFProbeVideoInfo, VideoProbeInput};
    use async_trait::async_trait;
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities::{IsoType, MediaStreamType, VideoType};
    use ferrofin_model::entities_media::MediaStream;
    use ferrofin_model::media_info::MediaProtocol;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

    /// The `DummyChapterDuration` the xUnit fixture injects:
    /// `TimeSpan.FromMinutes(5).TotalSeconds` == 300 seconds.
    const DUMMY_CHAPTER_DURATION_SECONDS: i32 = 5 * 60;

    /// One .NET minute, in ticks (`TimeSpan.TicksPerMinute`).
    const TICKS_PER_MINUTE: i64 = 60 * 10_000_000;

    /// A fake ffprobe seam. For the sync parity tests (`create_dummy_chapters`,
    /// `fetch`) probing is never reached. The `get_media_info` seam records the
    /// request it was handed and replays a canned [`MediaSourceInfo`], so the
    /// [`FFProbeVideoInfo::get_media_info`] arg-building path can be asserted
    /// without touching real subprocess I/O. The remaining encoder methods are
    /// the un-mockable subprocess surface and stay unreachable stubs.
    #[derive(Default)]
    struct FakeEncoder {
        /// The request captured by the most recent `get_media_info` call.
        last_request: std::sync::Mutex<Option<MediaInfoRequest>>,
        /// If set, `get_media_info` replays this instead of the default.
        canned: Option<MediaSourceInfo>,
    }

    #[async_trait]
    impl MediaEncoder for FakeEncoder {
        fn encoder_path(&self) -> String {
            String::new()
        }
        fn probe_path(&self) -> String {
            String::new()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(false)
        }
        async fn get_media_info(
            &self,
            request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            *self.last_request.lock().unwrap() = Some(request.clone());
            Ok(self.canned.clone().unwrap_or_default())
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        fn get_input_argument(&self, _input_file: &str, _media_source: &MediaSourceInfo) -> String {
            String::new()
        }
        fn get_time_parameter(&self, _ticks: i64) -> String {
            String::new()
        }
        async fn convert_image(
            &self,
            _input_path: &str,
            _output_path: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn provider() -> FFProbeVideoInfo<'static> {
        // The fake has no state that outlives the test, so a leaked reference is
        // safe and lets the helper return a `'static` provider for the sync tests.
        let encoder: &'static FakeEncoder = Box::leak(Box::new(FakeEncoder::default()));
        FFProbeVideoInfo::new(encoder, DUMMY_CHAPTER_DURATION_SECONDS)
    }

    fn video(run_time_ticks: Option<i64>) -> VideoProbeInput {
        VideoProbeInput {
            run_time_ticks,
            ..VideoProbeInput::default()
        }
    }

    // ---- get_media_info builds the MediaInfoRequest and drives the seam ----

    /// The seam builds a request with `ExtractChapters = true`,
    /// `MediaIsAudio = false`, and copies the item's path / video / iso type onto
    /// the media source, defaulting the protocol to `File` when unset.
    #[tokio::test]
    async fn get_media_info_builds_request_and_defaults_protocol_to_file() {
        let encoder = FakeEncoder::default();
        let provider = FFProbeVideoInfo::new(&encoder, DUMMY_CHAPTER_DURATION_SECONDS);

        let item = VideoProbeInput {
            path: Some("/media/movie.mkv".to_string()),
            path_protocol: None, // unset → defaults to File
            video_type: Some(VideoType::VideoFile),
            iso_type: Some(IsoType::BluRay),
            ..VideoProbeInput::default()
        };

        let out = provider
            .get_media_info(&item)
            .await
            .expect("fake probe succeeds");
        // The fake returns its canned/default source.
        assert_eq!(out, MediaSourceInfo::default());

        let req = encoder
            .last_request
            .lock()
            .unwrap()
            .clone()
            .expect("request was captured");
        assert!(req.extract_chapters);
        assert!(!req.media_is_audio);
        assert_eq!(req.media_source.path.as_deref(), Some("/media/movie.mkv"));
        assert_eq!(req.media_source.protocol, MediaProtocol::File);
        assert_eq!(req.media_source.video_type, Some(VideoType::VideoFile));
        assert_eq!(req.media_source.iso_type, Some(IsoType::BluRay));
    }

    /// An explicit protocol on the item is preserved (not overridden by the File
    /// default), and the probe result is returned verbatim.
    #[tokio::test]
    async fn get_media_info_preserves_explicit_protocol_and_returns_probe() {
        let canned = MediaSourceInfo {
            container: Some("mkv".to_string()),
            ..MediaSourceInfo::default()
        };
        let encoder = FakeEncoder {
            canned: Some(canned.clone()),
            ..FakeEncoder::default()
        };
        let provider = FFProbeVideoInfo::new(&encoder, DUMMY_CHAPTER_DURATION_SECONDS);

        let item = VideoProbeInput {
            path: Some("http://host/stream".to_string()),
            path_protocol: Some(MediaProtocol::Http),
            ..VideoProbeInput::default()
        };

        let out = provider.get_media_info(&item).await.expect("probe ok");
        assert_eq!(out, canned);

        let req = encoder.last_request.lock().unwrap().clone().unwrap();
        assert_eq!(req.media_source.protocol, MediaProtocol::Http);
    }

    // ---- Transliteration of Jellyfin.Providers.Tests FFProbeVideoInfoTests ----

    /// `CreateDummyChapters_InvalidRuntime_ThrowsArgumentException`.
    #[test]
    fn create_dummy_chapters_invalid_runtime_throws() {
        for runtime in [Some(-1_i64), Some(i64::MIN), Some(i64::MAX)] {
            let err = provider()
                .create_dummy_chapters(&video(runtime))
                .expect_err("invalid runtime is rejected");
            // C# throws ArgumentException → we map to a bad-request ServiceError.
            let _ = err;
        }
    }

    /// `CreateDummyChapters_ValidRuntime_CorrectChaptersCount`.
    #[test]
    fn create_dummy_chapters_valid_runtime_correct_count() {
        let cases: [(Option<i64>, usize); 7] = [
            (None, 0),
            (Some(0), 0),
            (Some(1), 1),
            (Some(TICKS_PER_MINUTE * 3), 1),
            (Some(TICKS_PER_MINUTE * 5), 1),
            (Some(TICKS_PER_MINUTE * 5 + 1), 1),
            (Some(TICKS_PER_MINUTE * 50), 10),
        ];

        for (runtime, expected) in cases {
            let chapters = provider()
                .create_dummy_chapters(&video(runtime))
                .expect("valid runtime produces chapters");
            assert_eq!(chapters.len(), expected, "runtime = {runtime:?}");
        }
    }

    /// `CreateDummyChapters_PositiveRuntime_NoChapterBeyondRuntime`.
    #[test]
    fn create_dummy_chapters_positive_runtime_no_chapter_beyond_runtime() {
        for runtime in [
            1_i64,
            TICKS_PER_MINUTE * 3,
            TICKS_PER_MINUTE * 5,
            TICKS_PER_MINUTE * 5 + 1,
            TICKS_PER_MINUTE * 50 + 1,
        ] {
            let chapters = provider()
                .create_dummy_chapters(&video(Some(runtime)))
                .expect("positive runtime produces chapters");
            for chapter in &chapters {
                assert!(
                    chapter.start_position_ticks < runtime,
                    "chapter at {} must be < runtime {runtime}",
                    chapter.start_position_ticks
                );
            }
        }
    }

    // ---- fetch() applies probe result to the item ----

    #[test]
    fn fetch_applies_probe_result_and_indexes_streams() {
        let audio = MediaStream {
            stream_type: MediaStreamType::Audio,
            index: 99,
            ..MediaStream::default()
        };
        let video_stream = MediaStream {
            stream_type: MediaStreamType::Video,
            index: 99,
            width: Some(1920),
            height: Some(1080),
            ..MediaStream::default()
        };
        let subtitle = MediaStream {
            stream_type: MediaStreamType::Subtitle,
            index: 99,
            ..MediaStream::default()
        };

        let info = MediaSourceInfo {
            container: Some("mkv".to_string()),
            bitrate: Some(8_000_000),
            run_time_ticks: Some(TICKS_PER_MINUTE * 90),
            media_streams: vec![audio, video_stream, subtitle],
            ..MediaSourceInfo::default()
        };

        let mut item = VideoProbeInput::default();
        let provider = provider();
        let streams = provider.fetch(&mut item, Some(&info));

        assert_eq!(item.container.as_deref(), Some("mkv"));
        assert_eq!(item.total_bitrate, Some(8_000_000));
        assert_eq!(item.run_time_ticks, Some(TICKS_PER_MINUTE * 90));
        assert_eq!(item.width, 1920);
        assert_eq!(item.height, 1080);
        assert!(item.has_subtitles);
        // Streams re-indexed 0..n.
        assert_eq!(
            streams.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(item.default_video_stream_index, Some(1));
    }

    #[test]
    fn fetch_without_media_info_leaves_dimensions_zero() {
        let mut item = VideoProbeInput {
            width: 640,
            height: 480,
            ..VideoProbeInput::default()
        };
        let provider = provider();
        let streams = provider.fetch(&mut item, None);

        assert!(streams.is_empty());
        assert_eq!(item.width, 0);
        assert_eq!(item.height, 0);
        assert!(!item.has_subtitles);
        assert_eq!(item.default_video_stream_index, None);
    }
}
