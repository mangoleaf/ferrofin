//! Real-ffmpeg integration tests for the concrete [`TokioSegmentTranscoder`] and
//! the [`TranscodeManagerImpl`] `start_ffmpeg` / `wait_for_segment` / kill
//! orchestration driving it.
//!
//! These exercise the ONE un-mockable piece — the real `tokio::process` spawn +
//! stderr pump + wait/kill wrapper — against a live ffmpeg, end-to-end through
//! the same orchestration the HTTP layer uses. They are excluded from the
//! unit-coverage gate (integration `tests/` never count toward
//! `cargo llvm-cov -p ferrofin-mediaencoding`, and the concrete spawn module carries
//! a module-level `#![cfg_attr(coverage_nightly, coverage(off))]` carve-out)
//! and skip themselves unless `FERROFIN_FFMPEG_TESTS` is set
//! *and* both `ffmpeg` and `ffprobe` are on `PATH`, so ffmpeg-less CI stays green.
//!
//! Run with:
//! `FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-mediaencoding --test segment_transcode_ffmpeg`

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrofin_mediaencoding::transcoding::manager::StartFfMpegRequest;
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingJobInfo, NoopSessionReporter, SegmentTranscoder, SpawnRequest,
    TokioSegmentTranscoder, TranscodeDisplayNames, TranscodeManagerImpl,
};
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_traits::media_encoding::TranscodingJobType;

/// Whether a program is on `PATH` (via `<prog> -version`).
fn on_path(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the ffmpeg-gated suite should run: `FERROFIN_FFMPEG_TESTS` set AND both
/// `ffmpeg` and `ffprobe` present. Prints a skip line and returns `false`
/// otherwise so absent-ffmpeg CI stays green.
fn ffmpeg_gate() -> bool {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: FERROFIN_FFMPEG_TESTS not set");
        return false;
    }
    if !on_path("ffmpeg") {
        eprintln!("skipping: ffmpeg not found on PATH");
        return false;
    }
    if !on_path("ffprobe") {
        eprintln!("skipping: ffprobe not found on PATH");
        return false;
    }
    true
}

/// Generates a tiny 6-second `testsrc`+`sine` clip at `path` (matches the plan's
/// generator command).
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
    assert!(
        std::fs::metadata(path).is_ok_and(|m| m.len() > 0),
        "generated clip is empty"
    );
}

/// The mpegts HLS (VOD) args writing `out%d.ts` + `out.m3u8` under `out_dir`.
///
/// A keyframe is forced every 2s so the muxer can actually cut segments on this
/// synthetic clip (which otherwise has too few keyframes to segment).
fn hls_ts_args(clip: &Path, out_dir: &Path) -> Vec<String> {
    let playlist = out_dir.join("out.m3u8");
    let seg_pattern = out_dir.join("out%d.ts");
    vec![
        "-y".into(),
        "-i".into(),
        clip.to_string_lossy().into_owned(),
        "-c:v".into(),
        "libx264".into(),
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
        playlist.to_string_lossy().into_owned(),
    ]
}

/// The fMP4 (CMAF) HLS (VOD) args writing an `out-1.mp4` init segment +
/// `out%d.mp4` media segments + a version-7 `out.m3u8` under `out_dir`.
fn hls_fmp4_args(clip: &Path, out_dir: &Path) -> Vec<String> {
    let playlist = out_dir.join("out.m3u8");
    let seg_pattern = out_dir.join("out%d.mp4");
    vec![
        "-y".into(),
        "-i".into(),
        clip.to_string_lossy().into_owned(),
        "-c:v".into(),
        "libx264".into(),
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
        "-hls_segment_type".into(),
        "fmp4".into(),
        "-hls_fmp4_init_filename".into(),
        "out-1.mp4".into(),
        "-hls_segment_filename".into(),
        seg_pattern.to_string_lossy().into_owned(),
        playlist.to_string_lossy().into_owned(),
    ]
}

/// A [`SpawnRequest`] for `program`/`args` writing into `out_dir`.
fn spawn_req(program: &str, args: Vec<String>, out_dir: &Path) -> SpawnRequest {
    SpawnRequest {
        env: Vec::new(),
        program: program.to_owned(),
        arguments: args,
        working_dir: None,
        output_dir: out_dir.to_path_buf(),
        log_path: out_dir.join("ffmpeg.log"),
    }
}

/// An HLS [`EncodingJobInfo`] for the manager path: playlist at `output_path`,
/// blocking on `wait_for` until the first segment/target exists.
fn state(output_path: &Path, wait_for: &Path, segment_container: &str) -> EncodingJobInfo {
    EncodingJobInfo {
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
        output_file_path: output_path.to_string_lossy().into_owned(),
        input_container: None,
        is_input_video: true,
        subtitle_delivery_method: SubtitleDeliveryMethod::Encode,
        run_time_ticks: None,
        transcoding_type: TranscodingJobType::Hls,
        supported_video_codecs: Vec::new(),
        supported_audio_codecs: Vec::new(),
        segment_length_secs: 2,
        wait_for_path: Some(wait_for.to_path_buf()),
        segment_container: Some(segment_container.to_owned()),
        play_session_id: Some("sess".to_owned()),
        device_id: Some("dev".to_owned()),
    }
}

/// A [`StartFfMpegRequest`] for `state`/`output_path`/`args`, log alongside.
fn start_req<'a>(
    state: &'a EncodingJobInfo,
    output_path: &'a Path,
    args: Vec<String>,
) -> StartFfMpegRequest<'a> {
    StartFfMpegRequest {
        env: Vec::new(),
        program: "ffmpeg",
        state,
        output_path,
        arguments: args,
        log_path: output_path.with_extension("log"),
        working_dir: None,
    }
}

/// Counts files under `dir` whose extension equals `ext` (e.g. `"ts"`).
fn count_ext(dir: &Path, ext: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == ext))
        .count()
}

/// Counts the `#EXTINF` (segment-duration) lines in an HLS playlist body.
fn extinf_count(playlist: &str) -> usize {
    playlist
        .lines()
        .filter(|l| l.starts_with("#EXTINF"))
        .count()
}

/// Steps 1–5 + the VOD playlist assertions of the plan: drive the real
/// [`TranscodeManagerImpl::start_ffmpeg`] with a `wait_for_path=out0.ts` target
/// and the real [`TokioSegmentTranscoder`], then assert the first segment is a
/// non-empty file on disk, the job exited cleanly, and the finished VOD playlist
/// is valid (`#EXTM3U` / `#EXT-X-ENDLIST` / ≥1 `#EXTINF`) with ≥2 `.ts` segments.
#[tokio::test]
async fn manager_start_ffmpeg_ts_waits_for_first_segment_then_completes() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);

    // The orchestration creates the output dir itself (mirrors StartFfMpeg).
    let out_dir = tmp.path().join("session");
    let playlist = out_dir.join("out.m3u8");
    let first_segment = out_dir.join("out0.ts");

    let transcoder = TokioSegmentTranscoder::new();
    let manager = TranscodeManagerImpl::new(NoopSessionReporter);
    let st = state(&playlist, &first_segment, "ts");
    let args = hls_ts_args(&clip, &out_dir);

    // start_ffmpeg blocks until out0.ts exists (wait_for_path) or ffmpeg exits.
    let handle = manager
        .start_ffmpeg(&transcoder, start_req(&st, &playlist, args))
        .await
        .expect("start_ffmpeg");

    // Step 4: the wait target — the first segment — is a non-empty file.
    assert!(first_segment.exists(), "out0.ts should exist");
    assert!(
        std::fs::metadata(&first_segment).unwrap().len() > 0,
        "out0.ts should be non-empty"
    );
    assert_eq!(manager.active_job_count(), 1, "job registered");
    assert_eq!(handle.job_type, TranscodingJobType::Hls);

    // Step 5: let the VOD transcode finish, then assert a valid playlist.
    let served = manager.wait_for_segment(&handle, &playlist, 0).await;
    assert!(served, "segment 0 should be served");

    // Drive the process to completion by waiting for the whole VOD to finish:
    // poll until #EXT-X-ENDLIST is written (the muxer closes the playlist last).
    wait_until(Duration::from_secs(20), || {
        std::fs::read_to_string(&playlist).is_ok_and(|p| p.contains("#EXT-X-ENDLIST"))
    })
    .await;

    let playlist_body = std::fs::read_to_string(&playlist).expect("read playlist");
    assert!(
        playlist_body.contains("#EXTM3U"),
        "playlist: {playlist_body}"
    );
    assert!(
        playlist_body.contains("#EXT-X-ENDLIST"),
        "playlist: {playlist_body}"
    );
    assert!(
        extinf_count(&playlist_body) >= 1,
        "expected >=1 #EXTINF, got: {playlist_body}"
    );
    let segs = count_ext(&out_dir, "ts");
    assert!(segs >= 2, "expected >=2 .ts segments, got {segs}");

    // The stderr log captured ffmpeg output (has the command header).
    let log = std::fs::read_to_string(out_dir.join("out.log")).expect("read log");
    assert!(log.contains("ffmpeg"), "log missing command header");
}

/// Step 6 (fMP4 variant): a real fMP4 transcode via the concrete transcoder
/// yields an `out-1.mp4` init segment, `out0.mp4` media segments, and an
/// `#EXT-X-VERSION:7` playlist that maps the init segment.
#[tokio::test]
async fn real_fmp4_transcode_produces_init_and_versioned_playlist() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);
    let out_dir = tmp.path().join("fmp4");
    std::fs::create_dir_all(&out_dir).unwrap();

    let transcoder = TokioSegmentTranscoder::new();
    let req = spawn_req("ffmpeg", hls_fmp4_args(&clip, &out_dir), &out_dir);
    let child = transcoder.start_transcode(&req).await.expect("spawn");
    let code = child.wait().await;
    assert_eq!(code, 0, "fmp4 ffmpeg exited non-zero");

    // Init segment + first media segment exist and are non-empty.
    let init = out_dir.join("out-1.mp4");
    let seg0 = out_dir.join("out0.mp4");
    assert!(init.exists(), "fMP4 init segment out-1.mp4 missing");
    assert!(seg0.exists(), "fMP4 media segment out0.mp4 missing");
    assert!(
        std::fs::metadata(&init).unwrap().len() > 0,
        "init segment empty"
    );
    assert!(
        count_ext(&out_dir, "mp4") >= 3,
        "expected init + >=2 segments"
    );

    // Version-7 playlist that maps the init segment.
    let playlist = std::fs::read_to_string(out_dir.join("out.m3u8")).expect("read playlist");
    assert!(
        playlist.contains("#EXT-X-VERSION:7"),
        "expected version 7, got: {playlist}"
    );
    assert!(
        playlist.contains("#EXT-X-MAP:URI=\"out-1.mp4\""),
        "expected init map, got: {playlist}"
    );
    assert!(playlist.contains("#EXT-X-ENDLIST"), "playlist: {playlist}");
}

/// Step 7 (kill): a long realtime transcode is killed mid-flight through the
/// manager's `kill_and_remove(delete_files=true)`; the child exits, the job is
/// removed, and the partial segment files are cleaned from the cache dir.
#[tokio::test]
async fn manager_kill_stops_transcode_and_deletes_partial_files() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("live");
    let playlist = out_dir.join("out.m3u8");
    let first_segment = out_dir.join("out0.ts");

    // A long, realtime-paced source so the process stays alive to be killed.
    let args = {
        let seg = out_dir.join("out%d.ts");
        vec![
            "-y".into(),
            "-re".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc=duration=60:size=128x72:rate=10".into(),
            "-c:v".into(),
            "libx264".into(),
            "-force_key_frames".into(),
            "expr:gte(t,n_forced*2)".into(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            "2".into(),
            "-hls_list_size".into(),
            "0".into(),
            "-hls_segment_filename".into(),
            seg.to_string_lossy().into_owned(),
            playlist.to_string_lossy().into_owned(),
        ]
    };

    let transcoder = TokioSegmentTranscoder::new();
    let manager = TranscodeManagerImpl::new(NoopSessionReporter);
    let st = state(&playlist, &first_segment, "ts");

    let handle = manager
        .start_ffmpeg(&transcoder, start_req(&st, &playlist, args))
        .await
        .expect("start_ffmpeg");
    assert_eq!(manager.active_job_count(), 1);
    // The first segment (the wait target) exists — the job is live.
    assert!(first_segment.exists(), "first segment written");

    // Kill via the manager with delete_files=true.
    manager.kill_and_remove(&handle, true, false).await;

    assert_eq!(manager.active_job_count(), 0, "job removed after kill");
    // The partial files (sharing the playlist stem "out") were deleted.
    assert!(!playlist.exists(), "playlist should be deleted");
    assert!(!first_segment.exists(), "out0.ts should be deleted");
    assert_eq!(count_ext(&out_dir, "ts"), 0, "no .ts segments remain");
}

/// Step 8 (failure): bogus args (a nonexistent input) surface as a `start_ffmpeg`
/// error — ffmpeg exits non-zero before the wait target ever appears.
#[tokio::test]
async fn manager_start_ffmpeg_surfaces_nonzero_exit_as_error() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("bad");
    let playlist = out_dir.join("out.m3u8");
    let first_segment = out_dir.join("out0.ts");

    let args = vec![
        "-y".into(),
        "-i".into(),
        "/nonexistent/does-not-exist.mkv".into(),
        "-f".into(),
        "hls".into(),
        playlist.to_string_lossy().into_owned(),
    ];
    let transcoder = TokioSegmentTranscoder::new();
    let manager = TranscodeManagerImpl::new(NoopSessionReporter);
    let st = state(&playlist, &first_segment, "ts");

    let err = manager
        .start_ffmpeg(&transcoder, start_req(&st, &playlist, args))
        .await
        .expect_err("bogus input should fail");
    assert!(
        err.contains("exited with code") || err.contains("timed out"),
        "unexpected error: {err}"
    );
    // The failed job left nothing registered (OnTranscodeFailedToStart).
    assert_eq!(manager.active_job_count(), 0);
}

/// The concrete transcoder alone (no manager): a real mpegts HLS VOD transcode
/// produces ≥2 `.ts` segments and a valid, closed playlist, and reports a clean
/// exit through the [`TranscodeChild`] handle.
#[tokio::test]
async fn concrete_transcoder_hls_transcode_produces_segments_and_playlist() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let clip: PathBuf = tmp.path().join("clip.mp4");
    make_clip(&clip);
    let out_dir = tmp.path().join("hls");
    std::fs::create_dir_all(&out_dir).unwrap();

    let transcoder = TokioSegmentTranscoder::new();
    let req = spawn_req("ffmpeg", hls_ts_args(&clip, &out_dir), &out_dir);
    let child = transcoder.start_transcode(&req).await.expect("spawn");

    let code = child.wait().await;
    assert_eq!(code, 0, "ffmpeg exited non-zero");
    assert!(child.has_exited());
    assert_eq!(child.exit_code(), Some(0));

    let playlist = std::fs::read_to_string(out_dir.join("out.m3u8")).expect("read playlist");
    assert!(playlist.contains("#EXTM3U"), "playlist: {playlist}");
    assert!(playlist.contains("#EXT-X-ENDLIST"), "playlist: {playlist}");
    assert!(extinf_count(&playlist) >= 1, "expected >=1 #EXTINF");
    assert!(count_ext(&out_dir, "ts") >= 2, "expected >=2 .ts segments");

    let log = std::fs::read_to_string(out_dir.join("ffmpeg.log")).expect("read log");
    assert!(log.contains("ffmpeg"), "log missing command header");
}

/// The concrete transcoder's `kill` stops a running realtime transcode.
#[tokio::test]
async fn concrete_transcoder_kill_stops_running_transcode() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("live");
    std::fs::create_dir_all(&out_dir).unwrap();

    let args = vec![
        "-y".into(),
        "-re".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc=duration=60:size=128x72:rate=10".into(),
        "-c:v".into(),
        "libx264".into(),
        "-f".into(),
        "hls".into(),
        "-hls_time".into(),
        "2".into(),
        "-hls_list_size".into(),
        "0".into(),
        "-hls_segment_filename".into(),
        out_dir.join("out%d.ts").to_string_lossy().into_owned(),
        out_dir.join("out.m3u8").to_string_lossy().into_owned(),
    ];
    let transcoder = TokioSegmentTranscoder::new();
    let child = transcoder
        .start_transcode(&spawn_req("ffmpeg", args, &out_dir))
        .await
        .expect("spawn");

    child.kill().await.expect("kill");
    assert!(child.has_exited(), "child should be exited after kill");
}

/// The concrete transcoder reports a non-zero exit for bogus input.
#[tokio::test]
async fn concrete_transcoder_nonzero_exit_is_reported() {
    if !ffmpeg_gate() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("bad");
    std::fs::create_dir_all(&out_dir).unwrap();

    let args = vec![
        "-i".into(),
        "/nonexistent/does-not-exist.mkv".into(),
        out_dir.join("out.ts").to_string_lossy().into_owned(),
    ];
    let transcoder = TokioSegmentTranscoder::new();
    let child = transcoder
        .start_transcode(&spawn_req("ffmpeg", args, &out_dir))
        .await
        .expect("spawn");
    let code = child.wait().await;
    assert_ne!(code, 0, "expected non-zero exit for bogus input");
}

/// Polls `cond` every 100 ms until it returns `true` or `budget` elapses.
async fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
