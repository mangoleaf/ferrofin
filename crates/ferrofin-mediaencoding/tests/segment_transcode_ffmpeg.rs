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
//! and are `#[ignore]`d, so a default run lists them as skipped. Asked for, they
//! need both `ffmpeg` and `ffprobe` on `PATH`.
//!
//! Run with:
//! `cargo test -p ferrofin-mediaencoding --test segment_transcode_ffmpeg -- --include-ignored`

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_mediaencoding::transcoding::manager::StartFfMpegRequest;
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingJobInfo, NoopSessionReporter, SegmentTranscoder, SpawnRequest,
    TokioSegmentTranscoder, TranscodeDisplayNames, TranscodeManagerImpl,
};
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_traits::media_encoding::{TranscodeManager as _, TranscodingJobType};

/// Whether a program is on `PATH` (via `<prog> -version`).
fn on_path(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Panics unless `ffmpeg` and `ffprobe` are on `PATH`: these tests are
/// `#[ignore]`d, so a run that asks for them must not pass without the tools.
fn require_ffmpeg() {
    for program in ["ffmpeg", "ffprobe"] {
        assert!(on_path(program), "{program} not found on PATH");
    }
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
        trimmed_subtitle_path: None,
        transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
        run_time_ticks: None,
        transcoding_type: TranscodingJobType::Hls,
        supported_video_codecs: Vec::new(),
        supported_audio_codecs: Vec::new(),
        supported_subtitle_codecs: Vec::new(),
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
        hardware_acceleration_type: ferrofin_model::entities::HardwareAccelerationType::none,
        user_id: None,
        program: "ffmpeg",
        state,
        output_path,
        arguments: args,
        log_dir: output_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default(),
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
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn manager_start_ffmpeg_ts_waits_for_first_segment_then_completes() {
    require_ffmpeg();
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

    // Step 4: start_ffmpeg waits for creation; ffmpeg may not have flushed
    // bytes yet. Wait for data before checking the first segment's contents.
    assert!(first_segment.exists(), "out0.ts should exist");
    wait_until(Duration::from_secs(20), || {
        std::fs::metadata(&first_segment).is_ok_and(|m| m.len() > 0)
    })
    .await;
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

    // The stderr log captured ffmpeg output (has the command header). It is
    // named like Jellyfin's (`FFmpeg.Transcode-<stamp>_<source>_<id>.log`)
    // inside `log_dir`, which `start_req` points at `out_dir`.
    let log_file = std::fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("FFmpeg.Transcode-"))
        })
        .expect("FFmpeg.Transcode-*.log in out_dir");
    let log = std::fs::read_to_string(&log_file).expect("read log");
    assert!(log.contains("ffmpeg"), "log missing command header");
}

/// Step 6 (fMP4 variant): a real fMP4 transcode via the concrete transcoder
/// yields an `out-1.mp4` init segment, `out0.mp4` media segments, and an
/// `#EXT-X-VERSION:7` playlist that maps the init segment.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn real_fmp4_transcode_produces_init_and_versioned_playlist() {
    require_ffmpeg();
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
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn manager_kill_stops_transcode_and_deletes_partial_files() {
    require_ffmpeg();
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
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn manager_start_ffmpeg_surfaces_nonzero_exit_as_error() {
    require_ffmpeg();
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
        err.to_string().contains("exited with code"),
        "unexpected error: {err}"
    );
    // The exited job stays registered, as upstream's `StartFfMpeg` leaves it
    // (`OnTranscodeFailedToStart` is for a process that never started), until
    // a stop report removes it (gl-1373).
    assert_eq!(manager.active_job_count(), 1);
    manager
        .kill_transcoding_jobs("dev", None, true)
        .await
        .expect("kill");
    assert_eq!(manager.active_job_count(), 0);
}

/// A loopback source that answers `500` to everything, and the request heads it
/// saw. The listener thread lives until the test binary exits.
fn failing_source() -> (u16, Arc<Mutex<String>>) {
    use std::io::{Read as _, Write as _};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(String::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = [0u8; 4096];
            let n = stream.read(&mut head).unwrap_or(0);
            record
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&head[..n]));
            let _ = stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        }
    });
    (port, seen)
}

/// The one `FFmpeg.*.log` a job left in `dir`.
fn ffmpeg_log(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().contains("FFmpeg."))
        .expect("the ffmpeg log")
}

/// A pass-through source's credentials reach ffmpeg, which sends them, and
/// nothing written about the job: its log (command line and ffmpeg's own
/// stderr) and the startup error, whose tail is read back out of that log.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_remote_source_reaches_ffmpeg_but_not_its_log_or_error() {
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = tmp.path().join("job");
    let playlist = out_dir.join("out.m3u8");
    let first_segment = out_dir.join("out0.ts");
    let (port, seen) = failing_source();
    let source =
        format!("http://INVENTED:SECRET@127.0.0.1:{port}/live/INVENTED/SECRET/1.m3u8?t=SECRET");
    let args = vec![
        "-y".into(),
        "-user_agent".into(),
        "Box/1.0 SECRET".into(),
        "-i".into(),
        source,
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
        .expect_err("a 500 source should fail")
        .to_string();
    assert!(err.contains("exited with code"), "{err}");
    assert!(!err.contains("SECRET"), "{err}");

    let log_path = ffmpeg_log(&out_dir);
    let finished = |text: &str| text.contains("Error opening input");
    wait_until(Duration::from_secs(5), || {
        finished(&std::fs::read_to_string(&log_path).unwrap_or_default())
    })
    .await;
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(finished(&log), "{log}");
    assert!(!log.contains("SECRET"), "{log}");
    let host = format!("http://127.0.0.1:{port}");
    assert!(log.contains(&format!("-i {host} ")), "{log}");
    assert!(log.contains(&format!("{host}.")), "{log}");

    let seen = seen.lock().unwrap();
    assert!(
        seen.contains("GET /live/INVENTED/SECRET/1.m3u8?t=SECRET HTTP/1.1"),
        "{seen}"
    );
    assert!(seen.contains("User-Agent: Box/1.0 SECRET"), "{seen}");
}

/// A file transcode's log is not filtered at all: a URL ffmpeg prints from the
/// file's own tags, and the command line, stay exactly as they were.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn an_ordinary_file_log_keeps_every_byte() {
    const TAG: &str = "https://someone:public@example.org/about?a=1";
    require_ffmpeg();
    let tmp = tempfile::tempdir().unwrap();
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);
    let tagged = tmp.path().join("tagged.mkv");
    let status = std::process::Command::new("ffmpeg")
        .arg("-i")
        .arg(&clip)
        .args(["-c", "copy", "-metadata"])
        .arg(format!("comment={TAG}"))
        .arg(&tagged)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());

    let log_path = tmp.path().join("job.log");
    let arguments = vec![
        "-i".to_owned(),
        format!("file:{}", tagged.display()),
        "-c".to_owned(),
        "copy".to_owned(),
        "-f".to_owned(),
        "null".to_owned(),
        "-".to_owned(),
    ];
    let header = format!("ffmpeg {}\n\n", arguments.join(" "));
    let req = SpawnRequest {
        program: "ffmpeg".to_owned(),
        arguments,
        working_dir: None,
        output_dir: tmp.path().to_path_buf(),
        log_path: log_path.clone(),
        env: Vec::new(),
    };
    let child = TokioSegmentTranscoder::new()
        .start_transcode(&req)
        .await
        .unwrap();
    assert_eq!(child.wait().await, 0);

    wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&log_path).is_ok_and(|log| log.contains("Output #0"))
    })
    .await;
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.starts_with(&header), "{log}");
    assert!(log.contains(TAG), "{log}");
}

/// The concrete transcoder alone (no manager): a real mpegts HLS VOD transcode
/// produces ≥2 `.ts` segments and a valid, closed playlist, and reports a clean
/// exit through the [`TranscodeChild`] handle.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn concrete_transcoder_hls_transcode_produces_segments_and_playlist() {
    require_ffmpeg();
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
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn concrete_transcoder_kill_stops_running_transcode() {
    require_ffmpeg();
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
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn concrete_transcoder_nonzero_exit_is_reported() {
    require_ffmpeg();
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

/// Statistics and interactive stdin are observed from a real ffmpeg process;
/// this does not assume that a stock binary implements Jellyfin's pause keys.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn concrete_transcoder_reports_stderr_progress_and_accepts_interactive_help() {
    require_ffmpeg();
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("progress.log");
    let req = SpawnRequest {
        program: "ffmpeg".to_owned(),
        arguments: vec![
            "-hide_banner".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc=duration=6:size=64x64:rate=10".into(),
            "-threads".into(),
            "1".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ],
        working_dir: None,
        output_dir: directory.path().to_path_buf(),
        log_path: log.clone(),
        env: Vec::new(),
    };
    let child = TokioSegmentTranscoder::new()
        .start_transcode(&req)
        .await
        .unwrap();
    child.write_stdin(b"?").await.unwrap();
    assert_eq!(child.wait().await, 0);
    for _ in 0..100 {
        if child
            .progress()
            .is_some_and(|progress| progress.elapsed_ticks.is_some_and(|ticks| ticks > 0))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let progress = child.progress().expect("actual ffmpeg stderr statistics");
    assert!(progress.elapsed_ticks.unwrap_or(0) > 0);
    assert!(progress.framerate.is_some());
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.contains("time="));
    // Inspecting help is harmless across stock and Jellyfin-ffmpeg; support
    // selection is measured separately by the startup capability probe.
}
