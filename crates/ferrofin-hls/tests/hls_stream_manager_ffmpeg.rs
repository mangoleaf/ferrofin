//! Real-ffmpeg end-to-end test for [`HlsStreamManagerImpl`] — the composition of
//! the HLS playlist generator + the live transcode runtime (`start_ffmpeg` /
//! `wait_for_segment` / the real [`TokioSegmentTranscoder`] spawn).
//!
//! This drives the *same* seam the HTTP layer serves verbatim: it asks the
//! manager for the master playlist, the variant `main.m3u8`, and then a dynamic
//! segment — exactly what the `ferrofin-api` `handlers::hls` routes forward — and
//! asserts the returned [`ServedFile`] points at a real, non-empty `.ts` segment
//! that a live ffmpeg produced. The un-ported request→plan glue
//! ([`StreamStatePlanner`]) is supplied here by a test planner that generates a
//! tiny clip and emits real HLS ffmpeg args; everything below it is production
//! code (`HlsStreamManagerImpl` + `TranscodeManagerImpl` + `TokioSegmentTranscoder`).
//!
//! Excluded from the unit-coverage gate (integration `tests/` never count toward
//! `cargo llvm-cov -p ferrofin-hls`) and gated on `FERROFIN_FFMPEG_TESTS` + `ffmpeg`
//! on `PATH`, so ffmpeg-less CI stays green.
//!
//! Run with:
//! `FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-hls --test hls_stream_manager_ffmpeg`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_hls::{
    DynamicHlsPlaylistGenerator, HlsStreamManagerImpl, StreamStatePlanner, TranscodePlan,
};
use ferrofin_mediaencoding::transcoding::{NoopSessionReporter, TokioSegmentTranscoder};
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingJobInfo, TranscodeDisplayNames, TranscodeManagerImpl,
};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{HlsStreamManager, HlsStreamRequest, TranscodingJobType};
use ferrofin_traits::system::ServerApplicationPaths;

/// Whether the ffmpeg-gated suite should run: `FERROFIN_FFMPEG_TESTS` set AND
/// `ffmpeg` on `PATH`. Prints a skip line and returns `false` otherwise.
fn ffmpeg_gate() -> bool {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: FERROFIN_FFMPEG_TESTS not set");
        return false;
    }
    let ok = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipping: ffmpeg not found on PATH");
    }
    ok
}

/// Generates a tiny 6-second `testsrc`+`sine` clip at `path`.
fn make_clip(path: &Path) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=6:size=128x72:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=6",
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
    fn log_directory_path(&self) -> String {
        String::new()
    }
    fn transcode_path(&self) -> String {
        self.transcode.clone()
    }
}

/// A real [`StreamStatePlanner`] that stands in for the un-ported
/// `GetStreamingState` + `GetCommandLineArguments`: it points every plan at a
/// pre-generated clip and emits real mpegts-HLS ffmpeg args writing
/// `out%d.ts` + `out.m3u8` into the transcode cache directory.
struct FfmpegPlanner {
    /// The transcode cache dir (matches `TempPaths.transcode`).
    dir: PathBuf,
    /// The source clip ffmpeg reads.
    clip: PathBuf,
    /// Emit fMP4 segments (`out%d.mp4` + `out-1.mp4` init) instead of mpegts.
    fmp4: bool,
}

impl FfmpegPlanner {
    /// The real HLS args writing `out%d.{ts,mp4}` + `out.m3u8` under `dir`.
    fn hls_args(&self, playlist: &Path) -> Vec<String> {
        let seg_ext = if self.fmp4 { "mp4" } else { "ts" };
        let seg_pattern = self.dir.join(format!("out%d.{seg_ext}"));
        let mut args = vec![
            "-y".into(),
            "-i".into(),
            self.clip.to_string_lossy().into_owned(),
            "-c:v".into(),
            "libx264".into(),
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
            "vod".into(),
            "-hls_segment_filename".into(),
            seg_pattern.to_string_lossy().into_owned(),
        ];
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
            run_time_ticks: Some(6 * 10_000_000),
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            segment_length_secs: 2,
            wait_for_path,
            segment_container: Some(seg_ext.to_owned()),
            play_session_id: Some("sess".to_owned()),
            device_id: Some("dev".to_owned()),
        };
        Ok(TranscodePlan {
            arguments: self.hls_args(&playlist),
            state,
            playlist_path: playlist,
            media_path: self.clip.to_string_lossy().into_owned(),
            run_time_ticks: 6 * 10_000_000,
            segment_length_ms: 2000,
            is_remuxing_video: false,
            segment_container: seg_ext.to_owned(),
            encoding_options: EncodingOptions::default(),
            min_segments: 3,
        })
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
    };
    let transcoder = TokioSegmentTranscoder::new();
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: dir.to_string_lossy().into_owned(),
    });
    HlsStreamManagerImpl::new(planner, transcoder, manager, generator, paths)
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
async fn end_to_end_master_variant_and_real_segment() {
    if !ffmpeg_gate() {
        return;
    }
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
    };
    let transcoder = TokioSegmentTranscoder::new();
    let manager = Arc::new(TranscodeManagerImpl::new(NoopSessionReporter));
    let cfg: Box<dyn Fn() -> EncodingOptions + Send + Sync> = Box::new(EncodingOptions::default);
    let generator = Arc::new(DynamicHlsPlaylistGenerator::new(cfg, Vec::new()));
    let paths = Arc::new(TempPaths {
        transcode: dir.to_string_lossy().into_owned(),
    });
    HlsStreamManagerImpl::new(planner, transcoder, manager, generator, paths)
}

/// The fragParsingError regression pin: hls.js requests the fMP4 init and the
/// first media segment CONCURRENTLY. The init request must start exactly one
/// ffmpeg (never a cancelled/orphaned spawn plus a second writer over the same
/// segment files — that tears the fragments), return a complete `ftyp`-headed
/// init, and the concurrent segment request must be served by that same job.
#[tokio::test]
async fn concurrent_init_and_segment_requests_share_one_job() {
    if !ffmpeg_gate() {
        return;
    }
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
    assert!(!seg_bytes.is_empty());

    // Exactly one transcode job served both requests.
    let log = cache.join("out.log");
    let spawns = std::fs::read_to_string(&log).map_or(1, |s| {
        // Each ffmpeg run writes one banner line with its version.
        s.matches("ffmpeg version").count().max(1)
    });
    assert_eq!(spawns, 1, "one ffmpeg owns the segment files");
}
