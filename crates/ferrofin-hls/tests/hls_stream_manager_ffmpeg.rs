//! Real-ffmpeg end-to-end test for [`HlsStreamManagerImpl`] — the composition of
//! the HLS playlist generator + the live transcode runtime (`start_ffmpeg` /
//! `wait_for_segment` / the real [`TokioSegmentTranscoder`] spawn).
//!
//! This drives the *same* seam the HTTP layer serves verbatim: it asks the
//! manager for the master playlist, the variant `main.m3u8`, and then a dynamic
//! segment — exactly what the `ferrofin-api` `handlers::hls` routes forward — and
//! asserts the returned [`ServedFile`] points at a real, non-empty `.ts` segment
//! that a live ffmpeg produced. The request→plan glue ([`StreamStatePlanner`],
//! whose real implementation lives in the server binary) is supplied here by a
//! test planner that generates a tiny clip and emits real HLS ffmpeg args;
//! everything below it is production code (`HlsStreamManagerImpl` +
//! `TranscodeManagerImpl` + `TokioSegmentTranscoder`).
//!
//! Excluded from the unit-coverage gate (integration `tests/` never count toward
//! `cargo llvm-cov -p ferrofin-hls`) and `#[ignore]`d, so a default run lists it
//! as skipped; asked for, it needs `ffmpeg` on `PATH`.
//!
//! Run with:
//! `cargo test -p ferrofin-hls --test hls_stream_manager_ffmpeg -- --include-ignored`

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_hls::{
    DynamicHlsPlaylistGenerator, HlsStreamManagerImpl, PlaylistKind, StreamStatePlanner,
    TranscodePlan,
};
use ferrofin_mediaencoding::transcoding::{
    NoopSessionReporter, SegmentTranscoder, SpawnRequest, TokioSegmentTranscoder, TranscodeChild,
};
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingJobInfo, TranscodeDisplayNames, TranscodeManagerImpl,
};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{HlsStreamManager, HlsStreamRequest, TranscodingJobType};
use ferrofin_traits::system::ServerApplicationPaths;

/// Panics unless `ffmpeg` is on `PATH`: these tests are `#[ignore]`d, so a
/// run that asks for them must not pass without it.
fn require_ffmpeg() {
    let ok = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "ffmpeg not found on PATH");
}

/// Generates a tiny 6-second `testsrc`+`sine` clip at `path`.
fn make_clip(path: &Path) {
    make_clip_of(path, 6);
}

/// Generates a tiny `secs`-second `testsrc`+`sine` clip at `path`.
fn make_clip_of(path: &Path, secs: u32) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={secs}:size=128x72:rate=10"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
        ])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg for clip");
    assert!(status.success(), "clip generation failed");
}

/// A [`ServerApplicationPaths`] whose transcode directory is a fixed temp path.
struct TempPaths {
    transcode: String,
}

impl ServerApplicationPaths for TempPaths {
    fn root_folder_path(&self) -> String {
        String::new()
    }
    fn default_user_views_path(&self) -> String {
        String::new()
    }
    fn people_path(&self) -> String {
        String::new()
    }
    fn genre_path(&self) -> String {
        String::new()
    }
    fn music_genre_path(&self) -> String {
        String::new()
    }
    fn studio_path(&self) -> String {
        String::new()
    }
    fn year_path(&self) -> String {
        String::new()
    }
    fn artists_path(&self) -> String {
        String::new()
    }
    fn user_configuration_directory_path(&self) -> String {
        String::new()
    }
    fn internal_metadata_path(&self) -> String {
        String::new()
    }
    fn program_data_path(&self) -> String {
        String::new()
    }
    fn web_path(&self) -> String {
        String::new()
    }
    fn data_path(&self) -> String {
        String::new()
    }
    fn image_cache_path(&self) -> String {
        String::new()
    }
    fn cache_path(&self) -> String {
        String::new()
    }
    // The test's temp dir: ffmpeg's `FFmpeg.Transcode-*.log` goes there,
    // never into the working directory (the crate).
    fn log_directory_path(&self) -> String {
        Path::new(&self.transcode)
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn transcode_path(&self) -> String {
        self.transcode.clone()
    }
}

/// A real [`StreamStatePlanner`] that stands in for the server's
/// `FerrofinStreamStatePlanner` (`GetStreamingState` +
/// `GetCommandLineArguments`): it points every plan at a
/// pre-generated clip and emits real mpegts-HLS ffmpeg args writing
/// `out%d.ts` + `out.m3u8` into the transcode cache directory.
struct FfmpegPlanner {
    /// The transcode cache dir (matches `TempPaths.transcode`).
    dir: PathBuf,
    /// The source clip ffmpeg reads.
    clip: PathBuf,
    /// Emit fMP4 segments (`out%d.mp4` + `out-1.mp4` init) instead of mpegts.
    fmp4: bool,
    /// Encode at the source's pace (`-re`) and seek the input to the requested
    /// segment, so segment requests can outrun the encoder.
    realtime: bool,
}

impl FfmpegPlanner {
    /// The real HLS args writing `out%d.{ts,mp4}` + `out.m3u8` under `dir`.
    ///
    /// `kind` selects the same VOD/EVENT muxer options the production planner
    /// emits (`-hls_playlist_type`, `-hls_base_url`, and the mpegts-only
    /// `-flags -global_header`), so the event assertions below are about what a
    /// real ffmpeg actually writes.
    fn hls_args(
        &self,
        playlist: &Path,
        kind: PlaylistKind,
        segment_id: Option<i32>,
    ) -> Vec<String> {
        let event = kind == PlaylistKind::Event;
        let seg_ext = if self.fmp4 { "mp4" } else { "ts" };
        let seg_pattern = self.dir.join(format!("out%d.{seg_ext}"));
        let mut args = vec!["-y".to_owned()];
        if self.realtime {
            args.push("-re".into());
            // The input seek to the segment's start, 2 s per segment.
            if let Some(id) = segment_id.filter(|&id| id > 0) {
                args.extend(["-ss".into(), (id * 2).to_string()]);
            }
        }
        args.extend([
            "-i".into(),
            self.clip.to_string_lossy().into_owned(),
            "-c:v".into(),
            "libx264".into(),
        ]);
        // No lookahead, so a paced job's first segment takes its 2 s, not more.
        if self.realtime {
            args.extend(["-preset".into(), "ultrafast".into()]);
        }
        args.extend([
            // Force keyframes so the synthetic clip actually segments.
            "-force_key_frames".into(),
            "expr:gte(t,n_forced*2)".into(),
            "-c:a".into(),
            "aac".into(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            "2".into(),
            "-hls_list_size".into(),
            "0".into(),
            "-hls_playlist_type".into(),
            if event { "event".into() } else { "vod".into() },
            "-hls_segment_filename".into(),
            seg_pattern.to_string_lossy().into_owned(),
        ]);
        // The production planner numbers a job's segments from the one asked for.
        if let Some(id) = segment_id {
            args.extend(["-start_number".into(), id.to_string()]);
        }
        // As the production planner does on every HLS job: the serve path takes
        // a segment file's existence as its completeness, which only the
        // `temp_file` rename makes true (without it ffmpeg creates the file
        // empty and fills it a moment later, gl-759).
        args.extend(["-hls_flags".into(), "temp_file".into()]);
        if event {
            args.extend(["-hls_base_url".into(), "hls/out/".into()]);
            if !self.fmp4 {
                args.extend(["-flags".into(), "-global_header".into()]);
            }
        }
        if self.fmp4 {
            args.extend([
                "-hls_segment_type".into(),
                "fmp4".into(),
                "-hls_fmp4_init_filename".into(),
                "out-1.mp4".into(),
            ]);
        }
        args.push(playlist.to_string_lossy().into_owned());
        args
    }
}

#[async_trait]
impl StreamStatePlanner for FfmpegPlanner {
    async fn plan(
        &self,
        _request: &HlsStreamRequest,
        _is_audio: bool,
        segment_id: Option<i32>,
        kind: PlaylistKind,
    ) -> Result<TranscodePlan, ServiceError> {
        let playlist = self.dir.join("out.m3u8");
        // The wait target for a segment request is that segment's file; a plain
        // playlist request has none.
        let seg_ext = if self.fmp4 { "mp4" } else { "ts" };
        let wait_for_path = segment_id.map(|id| self.dir.join(format!("out{id}.{seg_ext}")));
        let state = EncodingJobInfo {
            display: TranscodeDisplayNames::default(),
            base_request: BaseEncodingJobOptions::default(),
            video_stream: None,
            audio_stream: None,
            subtitle_stream: None,
            media_source: MediaSourceInfo::default(),
            output_video_codec: None,
            output_audio_codec: None,
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: None,
            output_video_sync: None,
            output_file_path: playlist.to_string_lossy().into_owned(),
            input_container: None,
            is_input_video: true,
            subtitle_delivery_method: SubtitleDeliveryMethod::Encode,
            trimmed_subtitle_path: None,
            transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
            run_time_ticks: Some(6 * 10_000_000),
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            supported_subtitle_codecs: Vec::new(),
            segment_length_secs: 2,
            wait_for_path,
            segment_container: Some(seg_ext.to_owned()),
            play_session_id: Some("sess".to_owned()),
            device_id: Some("dev".to_owned()),
        };
        Ok(TranscodePlan {
            ffmpeg_env: Vec::new(),
            arguments: self.hls_args(&playlist, kind, segment_id),
            state,
            playlist_path: playlist,
            media_path: self.clip.to_string_lossy().into_owned(),
            run_time_ticks: 6 * 10_000_000,
            segment_length_ms: 2000,
            is_remuxing_video: false,
            segment_container: seg_ext.to_owned(),
            encoding_options: EncodingOptions::default(),
            min_segments: 3,
            is_dovi_removed: false,
            is_hdr10_plus_removed: false,
        })
    }

    async fn hls_output(
        &self,
        _request: &HlsStreamRequest,
        _is_audio: bool,
    ) -> Result<(PathBuf, String), ServiceError> {
        let seg_ext = if self.fmp4 { "mp4" } else { "ts" };
        Ok((self.dir.join("out.m3u8"), seg_ext.to_owned()))
    }
}

/// The concrete manager type this test composes (real transcoder + real registry).
type Mgr = HlsStreamManagerImpl<
    FfmpegPlanner,
    TokioSegmentTranscoder,
    Box<dyn Fn() -> EncodingOptions + Send + Sync>,
    NoopSessionReporter,
>;

/// Builds a real [`HlsStreamManagerImpl`] over `dir` reading `clip`.
fn build_manager(dir: &Path, clip: &Path) -> Mgr {
    let planner = FfmpegPlanner {
        dir: dir.to_path_buf(),
        clip: clip.to_path_buf(),
        fmp4: false,
        realtime: false,
    };
    let transcoder = TokioSegmentTranscoder::new();
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: dir.to_string_lossy().into_owned(),
    });
    HlsStreamManagerImpl::new(planner, transcoder, manager, generator, paths, "ffmpeg")
}

/// A request carrying the session/device the planner pins.
fn request() -> HlsStreamRequest {
    HlsStreamRequest {
        item_id: uuid::Uuid::from_u128(1),
        device_id: Some("dev".to_owned()),
        play_session_id: Some("sess".to_owned()),
        segment_container: Some("ts".to_owned()),
        query_string: "?deviceId=dev".to_owned(),
        ..HlsStreamRequest::default()
    }
}

/// End-to-end through the manager seam: master playlist → variant `main.m3u8` →
/// a dynamic segment served from a real ffmpeg transcode. Asserts the served
/// file is a real, non-empty `.ts` with the `video/mp2t` content type — the
/// bytes the HTTP layer would stream back.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn end_to_end_master_variant_and_real_segment() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);

    let mgr = build_manager(&cache, &clip);
    let req = request();

    // 1. Master playlist points at the single variant, carrying the query.
    let master = mgr.master_playlist(&req, false).await.expect("master");
    assert!(
        master.starts_with("#EXTM3U\n#EXT-X-STREAM-INF:"),
        "master: {master}"
    );
    // Upstream's master never carries a version tag (only the variant does).
    assert!(!master.contains("#EXT-X-VERSION"), "master: {master}");
    assert!(
        master.contains("main.m3u8?deviceId=dev"),
        "master: {master}"
    );

    // 2. The variant playlist is generated (real DynamicHlsPlaylistGenerator).
    let variant = mgr.variant_playlist(&req, false).await.expect("variant");
    assert!(variant.starts_with("#EXTM3U"), "variant: {variant}");

    // 3. A dynamic segment request starts the real transcode and serves the
    //    materialised segment file.
    let served = mgr
        .dynamic_segment(&req, 0, false)
        .await
        .expect("dynamic_segment");
    assert_eq!(served.content_type, "video/mp2t");
    assert!(served.path.ends_with("out0.ts"), "served: {}", served.path);

    // The served path is a real, non-empty file ffmpeg wrote.
    let bytes = std::fs::read(&served.path).expect("read served segment");
    assert!(!bytes.is_empty(), "served segment is empty");
    // mpegts segments start with the 0x47 sync byte.
    assert_eq!(bytes[0], 0x47, "not a valid mpegts segment");

    // 4. Stopping the encoding kills the job and deletes the partial files.
    mgr.stop_encoding(&req).await.expect("stop_encoding");
    assert!(
        !cache.join("out0.ts").exists(),
        "partial segment should be deleted after stop"
    );
}

/// Builds a real fMP4-flavored manager over `dir` reading `clip`.
fn build_manager_fmp4(dir: &Path, clip: &Path) -> Mgr {
    let planner = FfmpegPlanner {
        dir: dir.to_path_buf(),
        clip: clip.to_path_buf(),
        fmp4: true,
        realtime: false,
    };
    let transcoder = TokioSegmentTranscoder::new();
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: dir.to_string_lossy().into_owned(),
    });
    HlsStreamManagerImpl::new(planner, transcoder, manager, generator, paths, "ffmpeg")
}

/// The fragParsingError regression pin: hls.js requests the fMP4 init and the
/// first media segment CONCURRENTLY. The init request must start exactly one
/// ffmpeg (never a cancelled/orphaned spawn plus a second writer over the same
/// segment files — that tears the fragments), return a complete `ftyp`-headed
/// init, and the concurrent segment request must be served by that same job.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn concurrent_init_and_segment_requests_share_one_job() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);

    let mgr = build_manager_fmp4(&cache, &clip);
    let mut req = request();
    req.segment_container = Some("mp4".to_owned());

    let (init, seg0) = tokio::join!(
        mgr.dynamic_segment(&req, -1, false),
        mgr.dynamic_segment(&req, 0, false),
    );
    let init = init.expect("init segment served");
    let seg0 = seg0.expect("segment 0 served");

    // The init is a complete fMP4 header: non-empty, `ftyp` at offset 4.
    let init_bytes = std::fs::read(&init.path).expect("read init");
    assert!(init_bytes.len() > 8, "init is {} bytes", init_bytes.len());
    assert_eq!(&init_bytes[4..8], b"ftyp", "init is not an fMP4 header");
    let seg_bytes = std::fs::read(&seg0.path).expect("read seg0");
    assert!(!seg_bytes.is_empty(), "{seg_bytes:?}");

    // Exactly one transcode job served both requests.
    let log = cache.join("out.log");
    let spawns = std::fs::read_to_string(&log).map_or(1, |s| {
        // Each ffmpeg run writes one banner line with its version.
        s.matches("ffmpeg version").count().max(1)
    });
    assert_eq!(spawns, 1, "one ffmpeg owns the segment files");
}

/// `live.m3u8` against a real ffmpeg: the served text is the EVENT playlist
/// ffmpeg wrote — its own `#EXT-X-TARGETDURATION`, its `hls/out/` segment URIs
/// (from `-hls_base_url`) — not something Ferrofin generated, and the segment
/// files those URIs name are on disk. A second request serves the same file
/// without starting a second transcode.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn live_playlist_serves_the_event_playlist_ffmpeg_wrote() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);

    let mgr = build_manager(&cache, &clip);
    let text = mgr.live_playlist(&request()).await.expect("live playlist");

    assert!(text.starts_with("#EXTM3U"), "live: {text}");
    assert!(
        text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
        "ffmpeg's event playlist: {text}"
    );
    // ffmpeg's own target duration for the 2s cuts — never a value Ferrofin
    // computed (the old implementation regenerated the variant playlist).
    assert!(
        text.contains("#EXT-X-TARGETDURATION:2"),
        "ffmpeg's TARGETDURATION: {text}"
    );
    // `-hls_base_url` routed the segment URIs through the `hls/{playlistId}/`
    // route, and the file each one names exists.
    assert!(text.contains("hls/out/out0.ts"), "routed URIs: {text}");
    assert!(cache.join("out0.ts").exists(), "segment 0 on disk");
    // The playlist is served verbatim: every non-tag line is a routed segment.
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        assert!(line.starts_with("hls/out/out"), "unexpected URI {line}");
    }

    // A second request finds the playlist on disk and serves the same bytes —
    // no regeneration. (That it also starts no second transcode is asserted by
    // the unit test `live_playlist_starts_event_job_and_serves_ffmpeg_written_file`,
    // which counts the fake transcoder's spawns; the log here is truncated by
    // each spawn, so it cannot carry that evidence.)
    let again = mgr.live_playlist(&request()).await.expect("live playlist");
    assert_eq!(again, text, "served verbatim on the second request");
}

/// The fMP4 event playlist: ffmpeg writes a bare `#EXT-X-MAP:URI="out-1.mp4"`
/// (it does not apply `-hls_base_url` to the init), and `GetLivePlaylistText`
/// rewrites exactly that one URI to the `hls/{playlistId}/` route while the
/// segment URIs (already routed by ffmpeg) are untouched.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn live_playlist_routes_the_real_fmp4_init_uri() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);

    let mgr = build_manager_fmp4(&cache, &clip);
    let mut req = request();
    req.segment_container = Some("mp4".to_owned());
    let text = mgr.live_playlist(&req).await.expect("live playlist");

    assert!(
        text.contains("#EXT-X-MAP:URI=\"hls/out/out-1.mp4\""),
        "init URI routed: {text}"
    );
    assert!(
        !text.contains("URI=\"out-1.mp4\""),
        "the bare init URI must not survive: {text}"
    );
    assert!(text.contains("hls/out/out0.mp4"), "routed segments: {text}");
    assert!(cache.join("out-1.mp4").exists(), "init written");
    assert!(cache.join("out0.mp4").exists(), "segment 0 written");
}

/// The real transcoder, recording each spawn's arguments.
struct RecordingTranscoder {
    inner: TokioSegmentTranscoder,
    spawns: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl SegmentTranscoder for RecordingTranscoder {
    async fn start_transcode(&self, req: &SpawnRequest) -> Result<Box<dyn TranscodeChild>, String> {
        self.spawns.lock().unwrap().push(req.arguments.clone());
        self.inner.start_transcode(req).await
    }
}

/// gl-52 against a real ffmpeg encoding at the source's pace: play 0 to 2,
/// seek forward to 20, play 21, seek back to 10, play 11 and 12. The killed
/// jobs' segments stay on disk (20 onward, past the job started at 10), but the
/// running job's front is its own: only the start and the two seeks spawn an
/// ffmpeg, and every sequential read waits on the running job. The jumps are
/// wide because a killed ffmpeg keeps writing through its graceful `q` exit.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_seek_back_below_a_killed_jobs_segments_keeps_the_running_job() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip_of(&clip, 60);

    let planner = FfmpegPlanner {
        dir: cache.clone(),
        clip,
        fmp4: false,
        realtime: true,
    };
    let spawns = Arc::new(Mutex::new(Vec::new()));
    let transcoder = RecordingTranscoder {
        inner: TokioSegmentTranscoder::new(),
        spawns: Arc::clone(&spawns),
    };
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: cache.to_string_lossy().into_owned(),
    });
    let mgr = HlsStreamManagerImpl::new(planner, transcoder, manager, generator, paths, "ffmpeg");

    let req = request();
    for segment in [0, 1, 2, 20, 21, 10, 11, 12] {
        let served = mgr
            .dynamic_segment(&req, segment, false)
            .await
            .unwrap_or_else(|error| panic!("segment {segment}: {error:?}"));
        assert!(
            served.path.ends_with(&format!("out{segment}.ts")),
            "segment {segment} served {}",
            served.path
        );
    }

    let start_numbers: Vec<String> = spawns
        .lock()
        .unwrap()
        .iter()
        .filter_map(|args| {
            args.windows(2)
                .find(|pair| pair[0] == "-start_number")
                .map(|pair| pair[1].clone())
        })
        .collect();
    assert_eq!(start_numbers, ["0", "20", "10"], "one ffmpeg per seek");
    mgr.stop_encoding(&req).await.expect("stop_encoding");
}

/// gl-1520 against a real ffmpeg encoding at the source's pace: play 0, seek
/// forward to 5 and play 6 and 7, then seek back to 4, whose request the
/// client drops while the job encodes it, and retry it. The killed job's 5 to 7
/// adjoin 4, and ffmpeg writes nothing of 4 until it is finished, so the retry
/// waits on the job started at 4 rather than restarting it.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_retry_of_a_slow_first_segment_keeps_the_running_job() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip_of(&clip, 60);

    let planner = FfmpegPlanner {
        dir: cache.clone(),
        clip,
        fmp4: false,
        realtime: true,
    };
    let spawns = Arc::new(Mutex::new(Vec::new()));
    let transcoder = RecordingTranscoder {
        inner: TokioSegmentTranscoder::new(),
        spawns: Arc::clone(&spawns),
    };
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: cache.to_string_lossy().into_owned(),
    });
    let mgr = HlsStreamManagerImpl::new(planner, transcoder, manager, generator, paths, "ffmpeg");

    let req = request();
    for segment in [0, 5, 6, 7] {
        mgr.dynamic_segment(&req, segment, false)
            .await
            .unwrap_or_else(|error| panic!("segment {segment}: {error:?}"));
    }
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        mgr.dynamic_segment(&req, 4, false),
    )
    .await;
    assert!(dropped.is_err(), "segment 4 was answered: {dropped:?}");
    let served = mgr
        .dynamic_segment(&req, 4, false)
        .await
        .expect("segment 4");
    assert!(served.path.ends_with("out4.ts"), "served {}", served.path);

    let start_numbers: Vec<String> = spawns
        .lock()
        .unwrap()
        .iter()
        .filter_map(|args| {
            args.windows(2)
                .find(|pair| pair[0] == "-start_number")
                .map(|pair| pair[1].clone())
        })
        .collect();
    assert_eq!(start_numbers, ["0", "5", "4"], "the retry restarted");
    mgr.stop_encoding(&req).await.expect("stop_encoding");
}

/// The duration ffprobe reads from the media file at `path`, 0 when it reads none.
fn probed_duration(path: &Path) -> f64 {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

/// gl-1396 against a real ffmpeg encoding at the source's pace: a seek kills
/// the job partway through a segment, and its graceful `q` exit finalises that
/// segment cut short under its normal name. The restart deletes it, so every
/// finished segment left on disk is whole and none is served short.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_seek_restart_leaves_no_cut_short_segment_on_disk() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("transcodes");
    std::fs::create_dir_all(&cache).unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip_of(&clip, 30);

    let planner = FfmpegPlanner {
        dir: cache.clone(),
        clip,
        fmp4: false,
        realtime: true,
    };
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: cache.to_string_lossy().into_owned(),
    });
    let mgr: Mgr = HlsStreamManagerImpl::new(
        planner,
        TokioSegmentTranscoder::new(),
        manager,
        generator,
        paths,
        "ffmpeg",
    );

    let req = request();
    for segment in [0, 1] {
        mgr.dynamic_segment(&req, segment, false)
            .await
            .unwrap_or_else(|error| panic!("segment {segment}: {error:?}"));
    }
    // Into the middle of a two-second segment, so the kill cuts one short.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let served = mgr
        .dynamic_segment(&req, 10, false)
        .await
        .expect("segment 10");
    assert!(served.path.ends_with("out10.ts"), "served {}", served.path);

    let mut segments: Vec<(String, f64)> = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("out") && Path::new(name).extension().is_some_and(|ext| ext == "ts")
        })
        .map(|name| {
            let duration = probed_duration(&cache.join(&name));
            (name, duration)
        })
        .collect();
    segments.sort_by(|a, b| a.0.cmp(&b.0));
    for name in ["out0.ts", "out1.ts", "out10.ts"] {
        assert!(
            segments.iter().any(|(segment, _)| segment == name),
            "{name} missing: {segments:?}"
        );
    }
    assert!(
        segments.iter().all(|&(_, duration)| duration > 1.5),
        "a segment on disk is cut short: {segments:?}"
    );
    mgr.stop_encoding(&req).await.expect("stop_encoding");
}
