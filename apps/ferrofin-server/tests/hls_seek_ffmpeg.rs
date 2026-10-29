//! A seeked HLS segment through a real ffmpeg, over HTTP: with the video
//! transcoded and the audio copied, a job started mid-file (a resume here; a
//! seek restarts the job with the same `-ss`) writes segments carrying both
//! streams, the audio starting with the video (gl-1018).
//!
//! Needs `ffmpeg` 5.0 or later (with libx264) and `ffprobe` on `PATH`:
//!
//! ```bash
//! FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-server --test hls_seek_ffmpeg
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

/// The real ffmpeg and ffprobe, or `None` (saying why) when this run skips:
/// `FERROFIN_FFMPEG_TESTS` unset, a tool missing, no libx264, or an ffmpeg
/// older than 5.0 (its `noise` filter has no `drop`, so nothing is trimmed).
fn tools() -> Option<(PathBuf, PathBuf)> {
    if std::env::var("FERROFIN_FFMPEG_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set FERROFIN_FFMPEG_TESTS=1 to run");
        return None;
    }
    let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return None;
    };
    let encoders = Command::new(&ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&encoders.stdout).contains("libx264") {
        eprintln!("skipping: this ffmpeg has no libx264 to transcode to");
        return None;
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn a_seeked_segment_with_copied_audio_starts_both_streams_at_the_seek() {
    let Some((ffmpeg, ffprobe)) = tools() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let movies = root.join("media").join("movies");
    generate_movie(&ffmpeg, &movies.join("Movie (2020)"));

    // Bounded: a stalled transcode must fail the test, not hang it.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let (_server, base, _) = boot::spawn_server(&client, |port| Config {
        port,
        ffmpeg_path: Some(ffmpeg.clone()),
        ffprobe_path: Some(ffprobe.clone()),
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
    let (item, source) = loop {
        let items = api
            .get("/Items?Recursive=true&IncludeItemTypes=Movie&Fields=MediaSources")
            .await;
        let movie = &items["Items"][0];
        if movie["MediaSources"][0]["MediaStreams"]
            .as_array()
            .is_some_and(|streams| streams.len() == 2)
        {
            break (
                movie["Id"].as_str().unwrap().to_owned(),
                movie["MediaSources"][0]["Id"].as_str().unwrap().to_owned(),
            );
        }
        assert!(
            Instant::now() < deadline,
            "the movie was not scanned: {items}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

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
