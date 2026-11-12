//! A seeked HLS segment through a real ffmpeg, over HTTP: with the video
//! transcoded and the audio copied, a job started mid-file (a resume here; a
//! seek restarts the job with the same `-ss`) writes segments carrying both
//! streams, the audio starting with the video (gl-1018); and a seek-restarted
//! fMP4 segment carries the timestamps the job that played up to it wrote
//! (gl-1244).
//!
//! Needs `ffmpeg` 5.0 or later (with libx264) and `ffprobe` on `PATH`:
//!
//! ```bash
//! cargo test -p ferrofin-server --test hls_seek_ffmpeg -- --include-ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ferrofin_server::config::Config;
use serde_json::Value;

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str =
    r#"MediaBrowser Client="hls-seek-test", Device="fixture", DeviceId="hls-seek", Version="1""#;

/// The segment a resume at 12 s starts the job at (4 × 3 s), two seconds
/// past the keyframe at 10 s the copied audio is read from.
const SEGMENT: u32 = 4;

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The real ffmpeg and ffprobe, or `None` (saying why) when this run skips
/// because the ffmpeg is older than 5.0 (its `noise` filter has no `drop`, so
/// nothing is trimmed). The tests are `#[ignore]`d, so a run that asks for
/// them fails when a tool or libx264 is missing.
fn tools() -> Option<(PathBuf, PathBuf)> {
    let ffmpeg = tool("ffmpeg").expect("ffmpeg not found on PATH");
    let ffprobe = tool("ffprobe").expect("ffprobe not found on PATH");
    let encoders = Command::new(&ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&encoders.stdout).contains("libx264"),
        "this ffmpeg has no libx264 to transcode to"
    );
    let noise = Command::new(&ffmpeg)
        .args(["-hide_banner", "-h", "bsf=noise"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&noise.stdout)
        .lines()
        .any(|line| line.split_whitespace().next() == Some("-drop"))
    {
        eprintln!("skipping: this ffmpeg is older than 5.0 (no noise=drop)");
        return None;
    }
    Some((ffmpeg, ffprobe))
}

/// Thirty seconds of MPEG-4 Part 2 video at 10 fps with a keyframe every
/// 5 s, and AC-3 audio: a client asking for H.264 and AC-3 gets the video
/// transcoded and the audio copied.
fn generate_movie(ffmpeg: &Path, folder: &Path) {
    std::fs::create_dir_all(folder).unwrap();
    let status = Command::new(ffmpeg)
        .args(["-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=s=320x180:r=10:d=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=30"])
        .args(["-map", "0:v", "-map", "1:a"])
        .args(["-c:v", "mpeg4", "-g", "50", "-c:a", "ac3"])
        .arg(folder.join("Movie (2020).mkv"))
        .status()
        .unwrap();
    assert!(status.success(), "generating the movie");
}

/// Thirty seconds of H.264 at 24 fps with two frames of B-frame reorder
/// delay (`has_b_frames=2`) and a keyframe every 6 s, and AAC audio: a client
/// asking for H.264 and AAC gets both copied, in 6 s segments.
fn generate_b_frame_movie(ffmpeg: &Path, folder: &Path) {
    std::fs::create_dir_all(folder).unwrap();
    let status = Command::new(ffmpeg)
        .args(["-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=s=320x180:r=24:d=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=30"])
        .args(["-map", "0:v", "-map", "1:a"])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-bf", "3"])
        .args(["-g", "144", "-keyint_min", "144", "-sc_threshold", "0"])
        .args(["-c:a", "aac"])
        .arg(folder.join("Movie (2020).mkv"))
        .status()
        .unwrap();
    assert!(status.success(), "generating the movie");
}

/// The first presentation time of each stream in `path`, (video, audio).
fn first_times(ffprobe: &Path, path: &Path) -> (Option<f64>, Option<f64>) {
    let output = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "stream=index,codec_type"])
        .args([
            "-show_entries",
            "packet=stream_index,pts_time",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "probing {}", path.display());
    let probe: Value = serde_json::from_slice(&output.stdout).unwrap();
    let first = |kind: &str| {
        let index = probe["streams"]
            .as_array()?
            .iter()
            .find(|s| s["codec_type"] == kind)?["index"]
            .clone();
        probe["packets"]
            .as_array()?
            .iter()
            .filter(|p| p["stream_index"] == index)
            .filter_map(|p| p["pts_time"].as_str()?.parse::<f64>().ok())
            .reduce(f64::min)
    };
    (first("video"), first("audio"))
}

/// The top-level box types of an MP4 fragment.
fn boxes(mut bytes: &[u8]) -> Vec<String> {
    let mut kinds = Vec::new();
    while bytes.len() >= 8 {
        let size = usize::try_from(u32::from_be_bytes(bytes[..4].try_into().unwrap())).unwrap();
        kinds.push(String::from_utf8_lossy(&bytes[4..8]).into_owned());
        if size < 8 || size > bytes.len() {
            break;
        }
        bytes = &bytes[size..];
    }
    kinds
}

struct Api {
    client: reqwest::Client,
    base: String,
    auth: String,
}

impl Api {
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Vec<u8> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", &self.auth);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.bytes().await.unwrap().to_vec()
    }

    async fn get(&self, path: &str) -> Value {
        serde_json::from_slice(&self.send(reqwest::Method::GET, path, None).await).unwrap()
    }
}

/// A server on `root` with `movies` scanned as a movie library, logged in as
/// the seeded admin, with the id of the one movie and of its media source.
async fn serve_movies(
    root: &Path,
    movies: &Path,
    ffmpeg: &Path,
    ffprobe: &Path,
) -> (
    std::thread::JoinHandle<anyhow::Result<()>>,
    Api,
    String,
    String,
) {
    // Bounded: a stalled transcode must fail the test, not hang it.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let (server, base, _) = boot::spawn_server(&client, |port| Config {
        port,
        ffmpeg_path: Some(ffmpeg.to_owned()),
        ffprobe_path: Some(ffprobe.to_owned()),
        ..Config::test_stub(root)
    })
    .await;
    let mut api = Api {
        client,
        base,
        auth: CLIENT.to_owned(),
    };
    let login: Value = serde_json::from_slice(
        &api.send(
            reqwest::Method::POST,
            "/Users/AuthenticateByName",
            Some(&serde_json::json!({"Username": "admin", "Pw": ""})),
        )
        .await,
    )
    .unwrap();
    api.auth = format!(
        "{CLIENT}, Token=\"{}\"",
        login["AccessToken"].as_str().unwrap()
    );
    let options = serde_json::json!({
        "PathInfos": [{"Path": movies}],
        "EnableRealtimeMonitor": false,
        "EnableInternetProviders": false,
        "EnableChapterImageExtraction": false,
        "EnableTrickplayImageExtraction": false,
        "TypeOptions": [{"Type": "Movie", "MetadataFetchers": [], "ImageFetchers": []}]
    });
    api.send(
        reqwest::Method::POST,
        "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true",
        Some(&serde_json::json!({"LibraryOptions": options})),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let items = api
            .get("/Items?Recursive=true&IncludeItemTypes=Movie&Fields=MediaSources")
            .await;
        let movie = &items["Items"][0];
        if movie["MediaSources"][0]["MediaStreams"]
            .as_array()
            .is_some_and(|streams| streams.len() == 2)
        {
            return (
                server,
                api,
                movie["Id"].as_str().unwrap().to_owned(),
                movie["MediaSources"][0]["Id"].as_str().unwrap().to_owned(),
            );
        }
        assert!(
            Instant::now() < deadline,
            "the movie was not scanned: {items}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_seeked_segment_with_copied_audio_starts_both_streams_at_the_seek() {
    let Some((ffmpeg, ffprobe)) = tools() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let movies = root.join("media").join("movies");
    generate_movie(&ffmpeg, &movies.join("Movie (2020)"));

    let (_server, api, item, source) = serve_movies(root, &movies, &ffmpeg, &ffprobe).await;

    let start = u64::from(SEGMENT) * 30_000_000;
    for container in ["mp4", "ts"] {
        let query = format!(
            "mediaSourceId={source}&videoCodec=h264&audioCodec=ac3\
             &segmentContainer={container}&playSessionId=seek-{container}\
             &startTimeTicks={start}"
        );
        // Resuming, the init segment's request starts the job at the
        // segment holding the resume point, and that job writes both.
        let mut body = Vec::new();
        if container == "mp4" {
            body = api
                .send(
                    reqwest::Method::GET,
                    &format!(
                        "/Videos/{item}/hls1/main/-1.mp4?{query}\
                         &runtimeTicks=0&actualSegmentLengthTicks=0"
                    ),
                    None,
                )
                .await;
        }
        let media = api
            .send(
                reqwest::Method::GET,
                &format!(
                    "/Videos/{item}/hls1/main/{SEGMENT}.{container}?{query}\
                     &runtimeTicks={start}&actualSegmentLengthTicks=30000000"
                ),
                None,
            )
            .await;
        if container == "mp4" {
            let kinds = boxes(&media);
            assert!(
                kinds.iter().any(|k| k == "moof") && kinds.iter().any(|k| k == "mdat"),
                "the seeked fMP4 segment holds media: {kinds:?}"
            );
        }
        body.extend(media);
        let segment = root.join(format!("seek.{container}"));
        std::fs::write(&segment, body).unwrap();
        let (video, audio) = first_times(&ffprobe, &segment);
        let video = video.unwrap_or_else(|| panic!("{container}: no video in the segment"));
        let audio = audio.unwrap_or_else(|| panic!("{container}: no audio in the segment"));
        // Within a video frame (0.1 s) of each other: cut at the absolute
        // seek the audio was missing, untrimmed it starts at the keyframe,
        // two seconds early.
        assert!(
            (audio - video).abs() < 0.1,
            "{container}: video from {video} s, audio from {audio} s"
        );
        if container == "mp4" {
            assert!((video - 12.0).abs() < 0.1, "on the movie's clock: {video}");
        }
    }
}

/// fMP4 segment N carries the same timestamps whether the job that wrote it
/// played every segment before it or was restarted at N by a seek (gl-1244).
/// The copied video's first DTS is the reorder delay (83 ms here) below zero,
/// and without upstream's `-avoid_negative_ts disabled` the muxer shifted
/// every stream of the continuous job later by that much; a seek-restarted
/// job, already past zero, was never shifted.
///
/// TS segments still differ by the delay, as Jellyfin 12.4's do: ffmpeg's HLS
/// muxer writes each one through an mpegts muxer of its own, which shifts
/// them again, and only upstream's flag is ported (owner decision on
/// gl-1244). They are not compared here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_seek_restarted_fmp4_segment_keeps_the_continuous_jobs_timestamps() {
    /// The segment the seek restarts at: 18 s into the movie.
    const SEEK: u32 = 3;
    let Some((ffmpeg, ffprobe)) = tools() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let movies = root.join("media").join("movies");
    generate_b_frame_movie(&ffmpeg, &movies.join("Movie (2020)"));
    let (_server, api, item, source) = serve_movies(root, &movies, &ffmpeg, &ffprobe).await;

    let url = |session: &str, segment: i64, query: &str| {
        format!(
            "/Videos/{item}/hls1/main/{segment}.mp4?mediaSourceId={source}\
             &videoCodec=h264&audioCodec=aac&segmentContainer=mp4\
             &playSessionId={session}{query}"
        )
    };
    let at = |segment: u32| {
        format!(
            "&runtimeTicks={}&actualSegmentLengthTicks=60000000",
            u64::from(segment) * 60_000_000
        )
    };
    let get = |path: String| {
        let api = &api;
        async move { api.send(reqwest::Method::GET, &path, None).await }
    };

    // One job from the start: the init, then every segment up to N in order.
    let init = get(url(
        "continuous",
        -1,
        "&runtimeTicks=0&actualSegmentLengthTicks=0",
    ))
    .await;
    let mut segments = Vec::new();
    for segment in 0..=SEEK {
        segments.push(get(url("continuous", segment.into(), &at(segment))).await);
    }
    // Segment N first starts a job there, with the seek's `-ss`. A player
    // seeking mid-stream keeps the init it has, so that is the one read here.
    let seeked = get(url("seek", SEEK.into(), &at(SEEK))).await;

    let mut times = Vec::new();
    for (name, media) in [
        ("first", &segments[0]),
        ("continuous", segments.last().unwrap()),
        ("seek", &seeked),
    ] {
        let body = [init.as_slice(), media.as_slice()].concat();
        assert!(
            boxes(&body).iter().any(|k| k == "mdat"),
            "{name}: the segment holds media"
        );
        let path = root.join(format!("{name}.mp4"));
        std::fs::write(&path, body).unwrap();
        let (video, audio) = first_times(&ffprobe, &path);
        times.push((
            video.unwrap_or_else(|| panic!("{name}: no video")),
            audio.unwrap_or_else(|| panic!("{name}: no audio")),
        ));
    }
    let [(first, _), (video, audio), (seek_video, seek_audio)] = times[..] else {
        unreachable!()
    };
    // The one segment whose timestamps start below zero now, unshifted.
    assert!(first.abs() < 0.01, "segment 0's video from {first} s");
    // Within a quarter of a frame (the mp4 timescale rounds to 1 ms); the
    // shift was two frames.
    assert!(
        (video - seek_video).abs() < 0.01 && (audio - seek_audio).abs() < 0.01,
        "continuous video / audio from {video} / {audio} s, seek-restarted from {seek_video} / {seek_audio} s"
    );
    assert!((video - 18.0).abs() < 0.01, "on the movie's clock: {video}");
}
