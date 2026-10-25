//! Subtitles through a real ffmpeg, over HTTP: an embedded PGS track kept as
//! PGS in a progressive transcode, an external `.sup` sidecar embedded, and
//! burned into a seeked HLS segment, on the movie's clock; and an ASS track
//! burned in with the fonts its file ships, extracted into the cache.
//!
//! ffmpeg has no PGS encoder, so the subtitle is written here: display sets
//! drawing a white box in the middle of a black picture ([`SHOWINGS`]).
//! Needs `ffmpeg` 5.1 or later (with libx264) and `ffprobe` on `PATH`:
//!
//! ```bash
//! FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-server --test subtitle_streams_ffmpeg
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="subtitle-streams-test", Device="fixture", DeviceId="subtitle-streams", Version="1""#;

/// The admin password the server is seeded with.
const PASSWORD: &str = "subtitle-streams";

/// How long one HTTP request may take, transcodes of the 20 s movie included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The picture and the box the subtitle draws in its middle.
const WIDTH: u16 = 320;
const HEIGHT: u16 = 180;
const BOX: (u16, u16, u16, u16) = (128, 74, 64, 32);

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// A wrapper that appends its argument line to `calls.log` and runs the real
/// tool, so the test can read the command lines the server ran.
fn wrapper(root: &Path, name: &str, real: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join("bin").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let log = root.join("calls.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s %s\\n' '{name}' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        log.display(),
        real.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// One PGS segment: the `PG` magic, the presentation and decoding times at
/// 90 kHz, the segment type and the payload's length.
fn pgs_segment(kind: u8, pts: u32, payload: &[u8]) -> Vec<u8> {
    let mut segment = b"PG".to_vec();
    segment.extend(pts.to_be_bytes());
    segment.extend(0u32.to_be_bytes());
    segment.push(kind);
    segment.extend(u16::try_from(payload.len()).unwrap().to_be_bytes());
    segment.extend(payload);
    segment
}

/// A presentation composition for the whole picture: `objects` is empty for
/// the display set that clears the screen.
fn composition(number: u16, epoch_start: bool, objects: &[(u16, u16)]) -> Vec<u8> {
    let mut pcs = Vec::new();
    pcs.extend(WIDTH.to_be_bytes());
    pcs.extend(HEIGHT.to_be_bytes());
    pcs.push(0x10); // frame rate
    pcs.extend(number.to_be_bytes());
    pcs.push(if epoch_start { 0x80 } else { 0x00 });
    pcs.extend([0x00, 0x00]); // no palette update, palette 0
    pcs.push(u8::try_from(objects.len()).unwrap());
    for (x, y) in objects {
        pcs.extend(0u16.to_be_bytes()); // object 0
        pcs.extend([0x00, 0x00]); // window 0, not cropped
        pcs.extend(x.to_be_bytes());
        pcs.extend(y.to_be_bytes());
    }
    pcs
}

/// The one window, around the box.
fn window() -> Vec<u8> {
    let (x, y, width, height) = BOX;
    let mut wds = vec![0x01, 0x00];
    for value in [x, y, width, height] {
        wds.extend(value.to_be_bytes());
    }
    wds
}

/// When the box shows and hides, in 90 kHz ticks: 3.5-8 s and 10.5-14 s.
const SHOWINGS: [(u32, u32); 2] = [(315_000, 720_000), (945_000, 1_260_000)];

/// A PGS stream showing a white box at each of [`SHOWINGS`], as a `.sup`
/// file.
fn white_box_sup() -> Vec<u8> {
    const END: u8 = 0x80;
    let (x, y, width, height) = BOX;
    // The palette definition: palette 0, entry 1 white (Y, Cr, Cb), opaque.
    let palette = [0x00, 0x00, 0x01, 235, 128, 128, 255];
    // Each line one run of colour 1 (`00 11LLLLLL LLLLLLLL CC`), then the
    // end of the line (`00 00`).
    let mut rle = Vec::new();
    for _ in 0..height {
        rle.extend([0x00, 0xC0 | u8::try_from(width >> 8).unwrap()]);
        rle.extend([u8::try_from(width & 0xFF).unwrap(), 0x01, 0x00, 0x00]);
    }
    // The object definition: object 0, version 0, first and last fragment.
    let mut object = vec![0x00, 0x00, 0x00, 0xC0];
    let length = u32::try_from(rle.len() + 4).unwrap().to_be_bytes();
    object.extend(&length[1..]);
    object.extend(width.to_be_bytes());
    object.extend(height.to_be_bytes());
    object.extend(rle);

    let mut sup = Vec::new();
    for (number, (show, hide)) in (0u16..).step_by(2).zip(SHOWINGS) {
        sup.extend(pgs_segment(
            0x16,
            show,
            &composition(number, true, &[(x, y)]),
        ));
        sup.extend(pgs_segment(0x17, show, &window()));
        sup.extend(pgs_segment(0x14, show, &palette));
        sup.extend(pgs_segment(0x15, show, &object));
        sup.extend(pgs_segment(END, show, &[]));
        sup.extend(pgs_segment(
            0x16,
            hide,
            &composition(number + 1, false, &[]),
        ));
        sup.extend(pgs_segment(0x17, hide, &window()));
        sup.extend(pgs_segment(END, hide, &[]));
    }
    sup
}

/// A twenty-second black movie (long enough that a seek to 3 s is not
/// clamped five seconds before the end) with the PGS track embedded, and the
/// same track beside it as an English `.sup` sidecar.
fn generate_movie(ffmpeg: &Path, folder: &Path) -> PathBuf {
    std::fs::create_dir_all(folder).unwrap();
    let sidecar = folder.join("Movie (2020).en.sup");
    std::fs::write(&sidecar, white_box_sup()).unwrap();
    let movie = folder.join("Movie (2020).mkv");
    let status = Command::new(ffmpeg)
        .args(["-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("color=c=black:s={WIDTH}x{HEIGHT}:r=10:d=20"))
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20"])
        .arg("-i")
        .arg(&sidecar)
        // Without it the `.sup`'s timeline would start at its first cue.
        .arg("-copyts")
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:s"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "10"])
        .args(["-c:a", "aac", "-c:s", "copy"])
        .arg(&movie)
        .status()
        .unwrap();
    assert!(status.success(), "generating the movie");
    movie
}

/// The real `ferrofin-server` binary, one process per test.
struct Server {
    client: reqwest::Client,
    base: String,
    auth: String,
    process: std::process::Child,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A failed assertion must not leave the server running, and its log
        // goes with the temp dir: show the end of it first.
        let _ = self.process.kill();
        let _ = self.process.wait();
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            eprintln!(
                "server log tail:\n{}",
                lines[lines.len().saturating_sub(40)..].join("\n")
            );
        }
    }
}

impl Server {
    /// Starts the binary on a free port (another if a concurrent test took it
    /// first) and waits until this process, known by a unique `ServerName`,
    /// answers there, then logs in.
    async fn boot(root: &Path, ffmpeg: &Path, ffprobe: &Path) -> Self {
        let log_path = root.join("server.log");
        for _ in 0..boot::BOOT_ATTEMPTS {
            let port = boot::free_port();
            let name = boot::unique_name("ferrofin-subtitle-streams");
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .unwrap();
            let logged_before = log.metadata().unwrap().len();
            let mut command = Command::new(env!("CARGO_BIN_EXE_ferrofin-server"));
            // Nothing from the developer's shell: a `FERROFIN_CONFIG_DIR` would
            // send this test's config writes to a real install.
            for (key, _) in std::env::vars_os() {
                let key = key.to_string_lossy();
                if key.starts_with("FERROFIN_") || key.starts_with("OTEL_") {
                    command.env_remove(key.as_ref());
                }
            }
            let process = command
                .arg("--data-dir")
                .arg(root.join("data"))
                .args(["--bind", "127.0.0.1", "--port", &port.to_string()])
                .arg("--ffmpeg")
                .arg(ffmpeg)
                .env("FERROFIN_FFPROBE_PATH", ffprobe)
                .env("FERROFIN_ADMIN_PASSWORD", PASSWORD)
                .env("FERROFIN_SERVER_NAME", &name)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let mut server = Self {
                // Bounded: nextest has no slow-timeout, and a stalled
                // transcode would otherwise hang the run.
                client: reqwest::Client::builder()
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .unwrap(),
                base: format!("http://127.0.0.1:{port}"),
                auth: CLIENT.to_owned(),
                process,
                log: log_path.clone(),
            };
            let deadline = Instant::now() + boot::STARTUP_DEADLINE;
            let lost_the_port = loop {
                if boot::is_up(&server.client, &server.base, &name).await {
                    break false;
                }
                if let Some(status) = server.process.try_wait().unwrap() {
                    let log = std::fs::read(&log_path).unwrap();
                    let attempt = &log[usize::try_from(logged_before).unwrap()..];
                    assert!(
                        String::from_utf8_lossy(attempt)
                            .contains(&format!("failed to bind 127.0.0.1:{port}")),
                        "the server exited during boot: {status}"
                    );
                    break true;
                }
                assert!(Instant::now() < deadline, "server did not start");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            if lost_the_port {
                continue;
            }
            let login = server
                .send(
                    reqwest::Method::POST,
                    "/Users/AuthenticateByName",
                    Some(&json!({"Username":"admin","Pw":PASSWORD})),
                )
                .await;
            server.auth = format!(
                "{CLIENT}, Token=\"{}\"",
                login["AccessToken"].as_str().unwrap()
            );
            return server;
        }
        panic!("no free port survived long enough to bind");
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Value {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", &self.auth);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap().error_for_status().unwrap();
        response.json().await.unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str) -> Value {
        self.send(reqwest::Method::GET, path, None).await
    }

    /// The body of `path`, which must answer 200.
    async fn download(&self, path: &str) -> Vec<u8> {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        response.bytes().await.unwrap().to_vec()
    }

    /// Runs the library scan and waits for it to finish.
    async fn scan(&self) {
        let task = |tasks: Value| {
            tasks
                .as_array()
                .unwrap()
                .iter()
                .find(|task| task["Key"] == "RefreshLibrary")
                .unwrap()
                .clone()
        };
        let before = task(self.get("/ScheduledTasks").await);
        self.send(
            reqwest::Method::POST,
            &format!("/ScheduledTasks/Running/{}", before["Id"].as_str().unwrap()),
            None,
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let after = task(self.get("/ScheduledTasks").await);
            if after["State"] == "Idle"
                && after["LastExecutionResult"]["EndTimeUtc"]
                    != before["LastExecutionResult"]["EndTimeUtc"]
            {
                return;
            }
            assert!(Instant::now() < deadline, "the scan did not finish");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// `ffprobe`'s streams of the file at `path`.
fn probe_streams(ffprobe: &Path, path: &Path) -> Vec<Value> {
    let output = Command::new(ffprobe)
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "probing {}", path.display());
    let probe: Value = serde_json::from_slice(&output.stdout).unwrap();
    probe["streams"].as_array().unwrap().clone()
}

/// When the first subtitle packet of the file at `path` is presented.
fn first_subtitle_time(ffprobe: &Path, path: &Path) -> f64 {
    let output = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "s", "-show_packets"])
        .args(["-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "probing {}", path.display());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .expect("a subtitle packet")
        .trim()
        .parse()
        .unwrap()
}

/// The mean brightness of the box's middle in the frame `seconds` into
/// `path`.
fn brightness_in_the_box(ffmpeg: &Path, path: &Path, seconds: &str) -> f64 {
    let (x, y, width, height) = BOX;
    let output = Command::new(ffmpeg)
        .args(["-loglevel", "error", "-i"])
        .arg(path)
        .args(["-ss", seconds, "-frames:v", "1", "-vf"])
        .arg(format!(
            "crop=8:8:{}:{}",
            x + width / 2 - 4,
            y + height / 2 - 4
        ))
        .args(["-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reading a frame of {}",
        path.display()
    );
    assert_eq!(output.stdout.len(), 64);
    output.stdout.iter().map(|&v| f64::from(v)).sum::<f64>() / 64.0
}

/// A six-second movie with an ASS subtitle track and two font attachments:
/// `Font.ttf`, and one whose `filename` tag climbs out of any folder it is
/// dumped in (`../../escape.ttf`). The fonts' bytes are not a real font
/// (libass falls back), the names are what is tested. The generator's inputs
/// stay in `scratch`, outside the library.
fn generate_fonts_movie(ffmpeg: &Path, scratch: &Path, folder: &Path) {
    std::fs::create_dir_all(scratch).unwrap();
    std::fs::create_dir_all(folder).unwrap();
    let ass = scratch.join("track.ass");
    std::fs::write(
        &ass,
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 180\n\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, Alignment\n\
         Style: Default,Shipped Font,24,&H00FFFFFF,2\n\n\
         [Events]\nFormat: Layer, Start, End, Style, Text\n\
         Dialogue: 0,0:00:00.50,0:00:05.00,Default,shipped\n",
    )
    .unwrap();
    let font = scratch.join("font.bin");
    std::fs::write(&font, b"not really a font").unwrap();
    let status = Command::new(ffmpeg)
        .args(["-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("color=c=black:s={WIDTH}x{HEIGHT}:r=10:d=6"))
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6"])
        .arg("-i")
        .arg(&ass)
        .arg("-attach")
        .arg(&font)
        .arg("-attach")
        .arg(&font)
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:s"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-c:s",
            "ass",
        ])
        .args(["-metadata:s:t:0", "mimetype=application/x-truetype-font"])
        .args(["-metadata:s:t:0", "filename=Font.ttf"])
        .args(["-metadata:s:t:1", "mimetype=application/x-truetype-font"])
        .args(["-metadata:s:t:1", "filename=../../escape.ttf"])
        .arg(folder.join("Fonts (2021).mkv"))
        .status()
        .unwrap();
    assert!(status.success(), "generating the fonts movie");
}

/// Every file named `name` under `dir`.
fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_named(&path, name));
        } else if path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        }
    }
    found
}

/// Whether `ffmpeg` can encode the generated media (H.264 and AAC).
fn can_generate(ffmpeg: &Path) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("libx264"))
}

/// Whether `ffmpeg` has the `-isync` input option (5.1 and later), which the
/// server puts external inputs on the movie's clock with.
fn has_input_sync(ffmpeg: &Path) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-h", "full"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("-isync"))
}

/// The real ffmpeg and ffprobe, or `None` (saying why) when this run skips:
/// `FERROFIN_FFMPEG_TESTS` unset, a tool missing, no libx264, or an ffmpeg
/// older than 5.1.
fn tools() -> Option<(PathBuf, PathBuf)> {
    if std::env::var("FERROFIN_FFMPEG_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set FERROFIN_FFMPEG_TESTS=1 to run");
        return None;
    }
    let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return None;
    };
    if !can_generate(&ffmpeg) {
        eprintln!("skipping: this ffmpeg has no libx264 to generate the media");
        return None;
    }
    if !has_input_sync(&ffmpeg) {
        eprintln!("skipping: this ffmpeg is older than 5.1 (no -isync)");
        return None;
    }
    Some((ffmpeg, ffprobe))
}

impl Server {
    /// Adds `folder` as a movies library with no remote providers, scans it,
    /// and returns its one movie with its media sources.
    async fn the_only_movie_in(&self, folder: &Path) -> Value {
        let options = json!({
            "PathInfos": [{"Path": folder}],
            "EnableRealtimeMonitor": false,
            "EnableInternetProviders": false,
            "EnableChapterImageExtraction": false,
            "EnableTrickplayImageExtraction": false,
            "TypeOptions": [{"Type": "Movie", "MetadataFetchers": [], "ImageFetchers": []}]
        });
        self.send(
            reqwest::Method::POST,
            "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=false",
            Some(&json!({"LibraryOptions": options})),
        )
        .await;
        self.scan().await;
        let items = self
            .get("/Items?Recursive=true&IncludeItemTypes=Movie&Fields=MediaSources")
            .await;
        items["Items"][0].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn bitmap_subtitles_embed_and_burn_in_from_the_movie_and_a_sidecar() {
    let Some((real_ffmpeg, real_ffprobe)) = tools() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let movies = root.join("media").join("movies");
    generate_movie(&real_ffmpeg, &movies.join("Movie (2020)"));
    let ffmpeg = wrapper(root, "ffmpeg", &real_ffmpeg);
    let ffprobe = wrapper(root, "ffprobe", &real_ffprobe);

    let server = Server::boot(root, &ffmpeg, &ffprobe).await;
    let movie = &server.the_only_movie_in(&movies).await;
    let item = movie["Id"].as_str().expect("the movie was scanned");
    let source = &movie["MediaSources"][0];
    let source_id = source["Id"].as_str().unwrap();
    let subtitle = |external: bool| {
        source["MediaStreams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["Type"] == "Subtitle" && s["IsExternal"] == external)
            .unwrap_or_else(|| panic!("an external={external} subtitle in {source}"))["Index"]
            .as_i64()
            .unwrap()
    };
    let (embedded, sidecar) = (subtitle(false), subtitle(true));

    // Embedding keeps the bitmap a bitmap, as the default track and at its
    // own time: the embedded one copied from the movie (gl-718), the sidecar
    // read as ffmpeg input 1 on the movie's clock (gl-106). Off that clock,
    // the sidecar's first cue would be moved to the start.
    for (index, what) in [(embedded, "embedded"), (sidecar, "sidecar")] {
        let body = server
            .download(&format!(
                "/Videos/{item}/stream.mkv?mediaSourceId={source_id}&static=false\
                 &videoCodec=h264&audioCodec=aac&playSessionId=embed-{what}\
                 &subtitleStreamIndex={index}&subtitleMethod=Embed"
            ))
            .await;
        let output = root.join(format!("embed-{what}.mkv"));
        std::fs::write(&output, body).unwrap();
        let streams = probe_streams(&real_ffprobe, &output);
        let subtitles: Vec<&Value> = streams
            .iter()
            .filter(|s| s["codec_type"] == "subtitle")
            .collect();
        assert_eq!(subtitles.len(), 1, "{what}: {streams:?}");
        assert_eq!(subtitles[0]["codec_name"], "hdmv_pgs_subtitle", "{what}");
        assert_eq!(subtitles[0]["disposition"]["default"], 1, "{what}");
        let first = first_subtitle_time(&real_ffprobe, &output);
        assert!((first - 3.5).abs() < 0.05, "{what}: first cue at {first}");
    }

    // Burning the sidecar into the segment that starts at 9 s: input 1 is
    // read from its start on the movie's clock, so the box drawn from 10.5 s
    // shows a second and a half into the segment, not before (gl-106, with
    // gl-786's single seek). Off the movie's clock the sidecar's timeline
    // would start at its first cue, 3.5 s, and the box would be on screen
    // from the segment's first frame; seeked as well, it is never drawn.
    let body = server
        .download(&format!(
            "/Videos/{item}/hls1/main/1.ts?mediaSourceId={source_id}\
             &videoCodec=h264&audioCodec=aac&segmentContainer=ts&segmentLength=9\
             &runtimeTicks=90000000&actualSegmentLengthTicks=90000000\
             &playSessionId=burn-sidecar&subtitleStreamIndex={sidecar}&subtitleMethod=Encode"
        ))
        .await;
    let segment = root.join("burn-sidecar-1.ts");
    std::fs::write(&segment, body).unwrap();
    let calls = std::fs::read_to_string(root.join("calls.log")).unwrap();
    let burn = calls
        .lines()
        .find(|line| {
            line.starts_with("ffmpeg ") && line.contains(".en.sup") && line.contains(" -f hls ")
        })
        .expect("the burn-in ran ffmpeg with the sidecar");
    // The movie seeked, the sidecar read whole on its clock, both decoded
    // onto the subtitle's canvas. (This ffmpeg has `-isync`: the test
    // needs 5.1 or later.)
    assert_eq!(burn.matches("-ss 00:00:09.000 ").count(), 1, "{burn}");
    assert!(
        burn.contains("-probesize 1G -canvas_size 320x180 -isync 0 -i file:"),
        "{burn}"
    );
    assert!(burn.contains("[1:0]"), "{burn}");
    let before = brightness_in_the_box(&real_ffmpeg, &segment, "1");
    let during = brightness_in_the_box(&real_ffmpeg, &segment, "2");
    assert!(
        before < 64.0 && during > 128.0,
        "the box is burned in from 10.5 s (means {before} at 10 s, {during} at 11 s)"
    );
}

/// Burning an ASS track in first extracts the fonts its file ships, and the
/// `subtitles` filter reads them there (gl-108). A font whose `filename` tag
/// is a path is written inside the cache folder all the same, by its leaf
/// (gl-414): nothing lands where the tag points.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burned_ass_track_reads_the_fonts_its_file_ships_from_the_cache() {
    let Some((real_ffmpeg, real_ffprobe)) = tools() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let movies = root.join("media").join("movies");
    generate_fonts_movie(
        &real_ffmpeg,
        &root.join("scratch"),
        &movies.join("Fonts (2021)"),
    );
    let ffmpeg = wrapper(root, "ffmpeg", &real_ffmpeg);
    let ffprobe = wrapper(root, "ffprobe", &real_ffprobe);

    let server = Server::boot(root, &ffmpeg, &ffprobe).await;
    let movie = server.the_only_movie_in(&movies).await;
    let item = movie["Id"].as_str().expect("the movie was scanned");
    let source = &movie["MediaSources"][0];
    let source_id = source["Id"].as_str().unwrap();
    let ass = source["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["Type"] == "Subtitle")
        .unwrap_or_else(|| panic!("an ASS track in {source}"))["Index"]
        .as_i64()
        .unwrap();

    let body = server
        .download(&format!(
            "/Videos/{item}/hls1/main/0.ts?mediaSourceId={source_id}\
             &videoCodec=h264&audioCodec=aac&segmentContainer=ts&segmentLength=3\
             &runtimeTicks=0&actualSegmentLengthTicks=30000000\
             &playSessionId=burn-ass&subtitleStreamIndex={ass}&subtitleMethod=Encode"
        ))
        .await;
    assert!(!body.is_empty());

    // The fonts were dumped one by one to paths of the server's choosing, as
    // a path-shaped name requires, and both landed in one cache folder.
    let calls = std::fs::read_to_string(root.join("calls.log")).unwrap();
    let dump = calls
        .lines()
        .find(|line| line.starts_with("ffmpeg ") && line.contains("-dump_attachment:"))
        .expect("the attachments were extracted");
    assert!(!dump.contains("-dump_attachment:t"), "{dump}");
    let fonts = files_named(root, "Font.ttf");
    let escaped = files_named(root, "escape.ttf");
    assert_eq!(fonts.len(), 1, "{fonts:?}");
    assert_eq!(escaped.len(), 1, "nothing outside the cache: {escaped:?}");
    let folder = fonts[0].parent().unwrap();
    assert_eq!(escaped[0].parent().unwrap(), folder);
    assert!(
        folder.components().any(|c| c.as_os_str() == "attachments"),
        "{}",
        folder.display()
    );

    // The burn-in reads them from there.
    let burn = calls
        .lines()
        .find(|line| line.starts_with("ffmpeg ") && line.contains(" -f hls "))
        .expect("the burn-in ran");
    assert!(
        burn.contains(&format!(":fontsdir='{}'", folder.display())),
        "{burn}"
    );
}
