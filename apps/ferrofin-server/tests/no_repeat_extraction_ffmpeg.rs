//! Chapter images, trickplay and Intro Skipper analysis are made once per
//! file: once a file has them, nothing a server does again — a library scan,
//! the three scheduled tasks, every choice of jellyfin-web's refresh dialog
//! but "replace trickplay images" (a deliberate redo), a restart — runs
//! ffmpeg on it, and a full refresh only re-probes it.
//!
//! Real ffmpeg behind a logging wrapper, on three generated episodes with
//! chapters and a shared intro. Needs `ffmpeg` and `ffprobe` on `PATH`:
//!
//! ```bash
//! FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-server --test no_repeat_extraction_ffmpeg
//! ```
//!
//! Intro Skipper's automatic analysis waits a minute or two after item
//! events (upstream's timers), so it is switched off here and its scheduled
//! task drives the same engine, whose record of the episodes each mode
//! analysed (and the segments it stored) keeps it from analysing a file
//! twice.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str =
    r#"MediaBrowser Client="no-repeat-test", Device="fixture", DeviceId="no-repeat", Version="1""#;

/// The episodes, each [`EPISODE_SECONDS`] long with three chapters.
const EPISODES: [&str; 3] = [
    "Show - S01E01.mkv",
    "Show - S01E02.mkv",
    "Show - S01E03.mkv",
];
const EPISODE_SECONDS: u32 = 120;

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// A wrapper that appends its argument line to `calls.log` and runs the real
/// tool.
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

/// Three episodes sharing a 30-second intro (the same noise), then
/// episode-specific noise; chapters at 0, 40 and 80 seconds.
fn generate_media(ffmpeg: &Path, season: &Path) {
    std::fs::create_dir_all(season).unwrap();
    let chapters = season.parent().unwrap().join("chapters.txt");
    std::fs::write(
        &chapters,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1\nSTART=0\nEND=40\ntitle=Opening\n\
         [CHAPTER]\nTIMEBASE=1/1\nSTART=40\nEND=80\ntitle=Middle\n\
         [CHAPTER]\nTIMEBASE=1/1\nSTART=80\nEND=120\ntitle=End\n",
    )
    .unwrap();
    for (seed, name) in (1..).zip(EPISODES) {
        let status = Command::new(ffmpeg)
            .args(["-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=size=160x90:rate=10"])
            .args(["-f", "lavfi", "-i", "anoisesrc=d=30:c=pink:r=22050:seed=42"])
            .args(["-f", "lavfi", "-i"])
            .arg(format!(
                "anoisesrc=d={}:c=pink:r=22050:seed={seed}",
                EPISODE_SECONDS - 30
            ))
            .arg("-i")
            .arg(&chapters)
            .args(["-filter_complex", "[1:a][2:a]concat=n=2:v=0:a=1[a]"])
            .args(["-map", "0:v", "-map", "[a]", "-map_chapters", "3"])
            .args(["-t", &EPISODE_SECONDS.to_string()])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
            .arg(season.join(name))
            .status()
            .unwrap();
        assert!(status.success(), "generating {name}");
    }
}

/// What the tools were asked to do with the media so far: ffmpeg runs on a
/// media file (chapter grabs, trickplay, Intro Skipper's analysis), and
/// ffprobe runs. Discovery's capability checks never name a media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Calls {
    ffmpeg: usize,
    ffprobe: usize,
}

fn calls(root: &Path, media: &Path) -> Calls {
    let log = std::fs::read_to_string(root.join("calls.log")).unwrap_or_default();
    let media = media.to_string_lossy();
    let count = |tool: &str| {
        log.lines()
            .filter(|line| line.starts_with(tool) && line.contains(media.as_ref()))
            .count()
    };
    Calls {
        ffmpeg: count("ffmpeg "),
        ffprobe: count("ffprobe "),
    }
}

/// The admin password the server is seeded with.
const PASSWORD: &str = "no-repeat";

/// The real `ferrofin-server` binary, one process per boot: the metrics
/// pipeline is process-global, so a second in-process boot would export no
/// scan metrics, and a restart should be one.
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
    /// first, as [`boot::spawn_server`] does in process) and waits until this
    /// process, known by a unique `ServerName`, answers there.
    async fn boot(root: &Path, ffmpeg: &Path, ffprobe: &Path) -> Self {
        let log_path = root.join("server.log");
        for _ in 0..boot::BOOT_ATTEMPTS {
            let port = boot::free_port();
            let name = boot::unique_name("ferrofin-no-repeat");
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .unwrap();
            let logged_before = log.metadata().unwrap().len();
            let mut command = Command::new(env!("CARGO_BIN_EXE_ferrofin-server"));
            // Nothing from the developer's shell: a `FERROFIN_CONFIG_DIR` or a
            // `FERROFIN_DATA_DIR/config.toml` would send this test's config
            // writes to a real install, an OTLP endpoint its traces to a collector.
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
                .env("FERROFIN_ENABLE_METRICS", "true")
                .env("FERROFIN_ADMIN_PASSWORD", PASSWORD)
                .env("FERROFIN_SERVER_NAME", &name)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let mut server = Self {
                client: reqwest::Client::new(),
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
                    // Only a lost port is worth another try; any other exit is
                    // the failure to report.
                    let log = std::fs::read(&log_path).unwrap();
                    let attempt = &log[usize::try_from(logged_before).unwrap()..];
                    // `listen::bind_all`'s context names the HTTP port, which
                    // tells it from a discovery bind failure.
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

    async fn post(&self, path: &str, body: Option<&Value>) {
        self.send(reqwest::Method::POST, path, body).await;
    }

    async fn task(&self, key: &str) -> Value {
        self.get("/ScheduledTasks")
            .await
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["Key"] == key)
            .unwrap_or_else(|| panic!("no task {key}"))
            .clone()
    }

    async fn run_task(&self, key: &str) {
        let before = self.task(key).await;
        self.post(
            &format!("/ScheduledTasks/Running/{}", before["Id"].as_str().unwrap()),
            None,
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let after = self.task(key).await;
            if after["State"] == "Idle"
                && after["LastExecutionResult"]["EndTimeUtc"]
                    != before["LastExecutionResult"]["EndTimeUtc"]
            {
                assert_eq!(
                    after["LastExecutionResult"]["Status"], "Completed",
                    "{after}"
                );
                return;
            }
            assert!(Instant::now() < deadline, "{key} did not finish: {after}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A `/metrics` series summed over its label sets, `None` when the
    /// export has no line for it.
    async fn metric(&self, name: &str) -> Option<f64> {
        let text = self
            .client
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let values: Vec<f64> = text
            .lines()
            .filter(|line| {
                line.strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with([' ', '{']))
            })
            .filter_map(|line| line.rsplit(' ').next()?.parse().ok())
            .collect();
        (!values.is_empty()).then(|| values.iter().sum())
    }

    /// Library scans ended so far, any result (every series is exported
    /// from boot, at 0).
    async fn scans_ended(&self) -> f64 {
        self.metric("ferrofin_library_scans_total")
            .await
            .expect("ferrofin_library_scans_total in /metrics")
    }

    /// Whether a library scan is running; the gauge is always exported.
    async fn scanning(&self) -> bool {
        self.metric("ferrofin_library_scan_in_progress")
            .await
            .expect("ferrofin_library_scan_in_progress in /metrics")
            > 0.0
    }

    /// `POST /Items/{id}/Refresh` as jellyfin-web's refresh dialog sends it,
    /// waiting until a library scan has ended since (any result) and none
    /// is running.
    async fn refresh(&self, item: &str, query: &str) {
        let before = self.scans_ended().await;
        self.post(
            &format!("/Items/{item}/Refresh?Recursive=true&RegenerateTrickplay=false&{query}"),
            None,
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.scans_ended().await <= before || self.scanning().await {
            assert!(Instant::now() < deadline, "refresh {query} did not finish");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn shutdown(mut self) {
        self.post("/System/Shutdown", None).await;
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.process.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "server did not stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// The refresh dialog's choices (`refreshdialog.js`), `RegenerateTrickplay`
/// off: on, it asks for the work again by design.
const REFRESH_MODES: [&str; 5] = [
    // "Scan for new and updated files".
    "MetadataRefreshMode=Default&ImageRefreshMode=Default&ReplaceAllMetadata=false&ReplaceAllImages=false",
    // "Search for missing metadata", without and with "Replace existing images".
    "MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=false&ReplaceAllImages=false",
    "MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=false&ReplaceAllImages=true",
    // "Replace all metadata", without and with "Replace existing images".
    "MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=true&ReplaceAllImages=false",
    "MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=true&ReplaceAllImages=true",
];

/// Every repeat path, after the first round of work: the three tasks and a
/// library scan, then each refresh mode on every library, series and an
/// episode of each. No ffmpeg runs on a media file; a full refresh
/// re-probes, which shows it reached the files (the chapter and trickplay
/// decisions follow the probe). Returns the log's counts at the end.
async fn assert_nothing_repeats(
    server: &Server,
    targets: &[String],
    root: &Path,
    media: &Path,
    baseline: Calls,
) -> Calls {
    // The round's fixed starting point is the first round's end, so the
    // boot (a restart's included) is checked too.
    assert_eq!(
        calls(root, media).ffmpeg,
        baseline.ffmpeg,
        "ffmpeg ran on the media before the round started"
    );
    for key in [
        "RefreshLibrary",
        "RefreshChapterImages",
        "RefreshTrickplayImages",
        "IntroSkipperDetectSegmentsTask",
    ] {
        let before = calls(root, media);
        server.run_task(key).await;
        let after = calls(root, media);
        assert_eq!(after, before, "{key} repeated work");
        assert_eq!(after.ffmpeg, baseline.ffmpeg, "{key} repeated ffmpeg work");
    }
    for target in targets {
        for mode in REFRESH_MODES {
            let before = calls(root, media);
            server.refresh(target, mode).await;
            let after = calls(root, media);
            assert_eq!(
                after.ffmpeg, baseline.ffmpeg,
                "{mode} on {target} repeated ffmpeg work"
            );
            if mode.starts_with("MetadataRefreshMode=FullRefresh") {
                assert!(
                    after.ffprobe > before.ffprobe,
                    "{mode} on {target} reached no file: {after:?}"
                );
            }
        }
    }
    calls(root, media)
}

/// Whether `ffmpeg` can encode the generated media (H.264 and AAC).
fn can_generate(ffmpeg: &Path) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("libx264"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn extraction_and_analysis_never_repeat_for_unchanged_files() {
    if std::env::var("FERROFIN_FFMPEG_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set FERROFIN_FFMPEG_TESTS=1 to run");
        return;
    }
    let (Some(real_ffmpeg), Some(real_ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    };
    if !can_generate(&real_ffmpeg) {
        eprintln!("skipping: this ffmpeg has no libx264 to generate the media");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let media = root.join("media");
    // Two libraries: one extracts chapter images and trickplay during the
    // scan, the other leaves both to the scheduled tasks (Jellyfin's
    // default), where a scan only reconciles what the tasks made.
    let (by_scan, by_task) = (media.join("scan"), media.join("tasks"));
    for library in [&by_scan, &by_task] {
        generate_media(&real_ffmpeg, &library.join("Show").join("Season 01"));
    }
    let ffmpeg = wrapper(root, "ffmpeg", &real_ffmpeg);
    let ffprobe = wrapper(root, "ffprobe", &real_ffprobe);

    let server = Server::boot(root, &ffmpeg, &ffprobe).await;
    // A blocking trickplay pass, so a scan's work is done when the scan is.
    let mut config = server.get("/System/Configuration").await;
    config["TrickplayOptions"]["ScanBehavior"] = json!("Blocking");
    config["TrickplayOptions"]["EnableHwAcceleration"] = json!(false);
    server.post("/System/Configuration", Some(&config)).await;
    let plugins = server.get("/Plugins").await;
    let intro_skipper = plugins
        .as_array()
        .unwrap()
        .iter()
        .find(|plugin| plugin["Name"] == "Intro Skipper")
        .expect("Intro Skipper is compiled in")["Id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut skipper = server
        .get(&format!("/Plugins/{intro_skipper}/Configuration"))
        .await;
    skipper["AutoDetectIntros"] = json!(false);
    // With cached fingerprints a repeated analysis runs no ffmpeg at all, so
    // only an uncached engine makes a repeat visible in the log.
    skipper["CacheFingerprints"] = json!(false);
    server
        .post(
            &format!("/Plugins/{intro_skipper}/Configuration"),
            Some(&skipper),
        )
        .await;
    for (name, path, during_scan) in [("Scan", &by_scan, true), ("Tasks", &by_task, false)] {
        // No image fetchers: the screen grabber and the embedded image
        // extractor would run ffmpeg on the media under "Replace existing
        // images", legitimately, and the log cannot tell them apart.
        let options = json!({
            "PathInfos": [{"Path": path}],
            "EnableRealtimeMonitor": false,
            "EnableInternetProviders": false,
            "EnableChapterImageExtraction": true,
            "ExtractChapterImagesDuringLibraryScan": during_scan,
            "EnableTrickplayImageExtraction": true,
            "ExtractTrickplayImagesDuringLibraryScan": during_scan,
            "TypeOptions": [
                {"Type": "Series", "MetadataFetchers": [], "ImageFetchers": []},
                {"Type": "Season", "MetadataFetchers": [], "ImageFetchers": []},
                {"Type": "Episode", "MetadataFetchers": [], "ImageFetchers": []}
            ]
        });
        server
            .post(
                &format!("/Library/VirtualFolders?name={name}&collectionType=tvshows&refreshLibrary=false"),
                Some(&json!({"LibraryOptions": options})),
            )
            .await;
    }

    // The first round: every file probed; the scan library's chapters
    // grabbed and trickplay extracted by the scan, the other's by the tasks;
    // both seasons analysed by the Intro Skipper task.
    let files = 2 * EPISODES.len();
    server.run_task("RefreshLibrary").await;
    let scanned = calls(root, &media);
    assert_eq!(scanned.ffprobe, files, "{scanned:?}");
    assert_eq!(
        scanned.ffmpeg,
        EPISODES.len() * 4,
        "three chapter grabs and one trickplay run per file of the scan library: {scanned:?}"
    );
    server.run_task("RefreshChapterImages").await;
    server.run_task("RefreshTrickplayImages").await;
    let extracted = calls(root, &media);
    assert_eq!(
        extracted.ffmpeg,
        files * 4,
        "the tasks did the other library's: {extracted:?}"
    );
    let per_library = || [calls(root, &by_scan).ffmpeg, calls(root, &by_task).ffmpeg];
    let before_analysis = per_library();
    server.run_task("IntroSkipperDetectSegmentsTask").await;
    let after_analysis = per_library();
    for (before, after) in before_analysis.iter().zip(after_analysis) {
        assert!(
            after >= before + EPISODES.len(),
            "both seasons were analysed: {before_analysis:?} → {after_analysis:?}"
        );
    }
    let analysed = calls(root, &media);

    let mut targets: Vec<String> = server
        .get("/Library/VirtualFolders")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|library| library["ItemId"].as_str().unwrap().to_owned())
        .collect();
    let shows = server
        .get("/Items?Recursive=true&IncludeItemTypes=Series,Episode&Fields=Path")
        .await;
    for library in [&by_scan, &by_task] {
        for kind in ["Series", "Episode"] {
            let item = shows["Items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| {
                    item["Type"] == kind
                        && item["Path"]
                            .as_str()
                            .is_some_and(|path| Path::new(path).starts_with(library))
                })
                .unwrap_or_else(|| panic!("a {kind} under {}", library.display()));
            targets.push(item["Id"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(
        targets.len(),
        6,
        "two libraries, series and episodes: {targets:?}"
    );

    // Work a refresh or a timer starts after the round's last check still
    // reaches the log before the server process has exited, so each round
    // is checked once more after its shutdown.
    let last = assert_nothing_repeats(&server, &targets, root, &media, analysed).await;
    server.shutdown().await;
    assert_eq!(
        calls(root, &media),
        last,
        "work after the round's last check"
    );

    // A restart keeps all of it: the records are persisted, not cached.
    let server = Server::boot(root, &ffmpeg, &ffprobe).await;
    let last = assert_nothing_repeats(&server, &targets, root, &media, analysed).await;
    server.shutdown().await;
    assert_eq!(
        calls(root, &media),
        last,
        "work after the round's last check"
    );
}
