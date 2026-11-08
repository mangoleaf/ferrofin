//! The image limits hold on a real server under a memory cap.
//!
//! In Rust a failed allocation ends the process, so a limit has to be checked
//! before the memory is asked for. Each test boots the real binary in a cgroup
//! with a hard memory cap and swap off, over a library of photos, and checks
//! that past a limit the client gets the original file and the server is still
//! there. Needs a systemd user session:
//!
//! ```bash
//! FERROFIN_CGROUP_TESTS=1 cargo test -p ferrofin-server --test image_limits_cgroup
//! ```
//!
//! CI does not run these. They take a few minutes in a debug build, where the
//! resizes they do run are slow.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str =
    r#"MediaBrowser Client="cgroup-test", Device="fixture", DeviceId="cgroup", Version="1""#;

/// The admin password the server is seeded with.
const PASSWORD: &str = "capped";

/// Writes a solid `width`×`height` 8-bit PNG of `channels` (1 gray, 4 RGBA), a
/// row at a time, so the pixels are never in this test's memory.
fn write_png(path: &Path, width: u32, height: u32, channels: u8) {
    fn chunk(file: &mut std::fs::File, kind: [u8; 4], data: &[u8]) {
        let mut crc = flate2::Crc::new();
        crc.update(&kind);
        crc.update(data);
        let length = u32::try_from(data.len()).expect("chunk length");
        file.write_all(&length.to_be_bytes()).expect("write");
        file.write_all(&kind).expect("write");
        file.write_all(data).expect("write");
        file.write_all(&crc.sum().to_be_bytes()).expect("write");
    }

    let row_bytes = u64::from(width) * u64::from(channels);
    let row = vec![0_u8; usize::try_from(row_bytes).expect("row") + 1];
    let mut compressed = Vec::new();
    let mut encoder = flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::best());
    for _ in 0..height {
        encoder.write_all(&row).expect("compress");
    }
    encoder.finish().expect("compress");

    let mut file = std::fs::File::create(path).expect("create fixture");
    file.write_all(b"\x89PNG\r\n\x1a\n").expect("write");
    let mut header = Vec::new();
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    let color_type = if channels == 4 { 6 } else { 0 };
    header.extend([8, color_type, 0, 0, 0]);
    chunk(&mut file, *b"IHDR", &header);
    chunk(&mut file, *b"IDAT", &compressed);
    chunk(&mut file, *b"IEND", b"");
}

/// The real `ferrofin-server` binary in its own capped scope.
struct CappedServer {
    client: reqwest::Client,
    base: String,
    auth: String,
    process: std::process::Child,
    unit: String,
    cgroup: PathBuf,
    log: PathBuf,
}

impl Drop for CappedServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &format!("{}.scope", self.unit)])
            .status();
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

impl CappedServer {
    /// Starts the binary under `memory_max` and signs in as the admin.
    async fn boot(root: &Path, memory_max: &str, settings: &[(&str, &str)]) -> Self {
        let port = boot::free_port();
        let name = boot::unique_name("ferrofin-cgroup");
        let unit = name.clone();
        let log_path = root.join("server.log");
        let log = std::fs::File::create(&log_path).expect("log");
        let mut command = Command::new("systemd-run");
        // Nothing from the developer's shell: a `FERROFIN_CONFIG_DIR` would
        // send this test's writes to a real install.
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if key.starts_with("FERROFIN_") || key.starts_with("OTEL_") {
                command.env_remove(key.as_ref());
            }
        }
        let process = command
            .args(["--user", "--scope", "--quiet"])
            .arg(format!("--unit={unit}"))
            .arg("-p")
            .arg(format!("MemoryMax={memory_max}"))
            .args(["-p", "MemorySwapMax=0"])
            .arg(env!("CARGO_BIN_EXE_ferrofin-server"))
            .arg("--data-dir")
            .arg(root.join("data"))
            .args(["--bind", "127.0.0.1", "--port", &port.to_string()])
            .env("FERROFIN_ADMIN_PASSWORD", PASSWORD)
            .env("FERROFIN_SERVER_NAME", &name)
            .envs(settings.iter().copied())
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("systemd-run");
        let mut server = Self {
            client: reqwest::Client::new(),
            base: format!("http://127.0.0.1:{port}"),
            auth: CLIENT.to_owned(),
            process,
            cgroup: PathBuf::new(),
            unit,
            log: log_path,
        };
        let deadline = Instant::now() + boot::STARTUP_DEADLINE;
        while boot::server_name_at(&server.client, &server.base)
            .await
            .as_deref()
            != Some(name.as_str())
        {
            assert!(Instant::now() < deadline, "the server never answered");
            assert!(server.is_running(), "the server exited during boot");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let control_group = Command::new("systemctl")
            .args(["--user", "show", "-p", "ControlGroup", "--value"])
            .arg(format!("{}.scope", server.unit))
            .output()
            .expect("systemctl");
        let control_group = String::from_utf8(control_group.stdout).expect("utf-8");
        server.cgroup = PathBuf::from(format!("/sys/fs/cgroup{}", control_group.trim()));
        let token = server.sign_in("admin", PASSWORD).await;
        server.auth = format!(r#"{CLIENT}, Token="{token}""#);
        server
    }

    fn is_running(&mut self) -> bool {
        matches!(self.process.try_wait(), Ok(None))
    }

    /// `oom_kill` of `memory.events`, the kills under the cap so far.
    fn oom_kills(&self) -> u64 {
        std::fs::read_to_string(self.cgroup.join("memory.events"))
            .expect("memory.events")
            .lines()
            .find_map(|line| line.strip_prefix("oom_kill "))
            .and_then(|count| count.parse().ok())
            .expect("oom_kill")
    }

    async fn sign_in(&self, user: &str, password: &str) -> String {
        let body: Value = self
            .client
            .post(format!("{}/Users/AuthenticateByName", self.base))
            .header("Authorization", CLIENT)
            .json(&json!({ "Username": user, "Pw": password }))
            .send()
            .await
            .expect("sign in")
            .json()
            .await
            .expect("sign-in body");
        body["AccessToken"].as_str().expect("token").to_owned()
    }

    /// Adds `folder` as a library and waits for the scan to find its `count`
    /// photos and their images. Returns each one's item id, by file name.
    async fn add_photos(&self, folder: &Path, count: usize) -> HashMap<String, String> {
        let status = self
            .client
            .post(format!(
                "{}/Library/VirtualFolders?name=Pictures&collectionType=homevideos&refreshLibrary=true",
                self.base
            ))
            .header("Authorization", &self.auth)
            .json(&json!({ "LibraryOptions": { "PathInfos": [{ "Path": folder }] } }))
            .send()
            .await
            .expect("add library")
            .status();
        assert!(status.is_success(), "add library: {status}");

        let deadline = Instant::now() + Duration::from_mins(3);
        loop {
            let photos = self.json("/Items?recursive=true&includeItemTypes=Photo&fields=Path");
            let photos: Value = photos.await;
            let found: HashMap<String, String> = photos["Items"]
                .as_array()
                .expect("items")
                .iter()
                .map(|item| {
                    let path = Path::new(item["Path"].as_str().expect("path"));
                    let name = path.file_name().expect("a file name");
                    (
                        name.to_string_lossy().into_owned(),
                        item["Id"].as_str().expect("id").to_owned(),
                    )
                })
                .collect();
            if found.len() == count && self.all_have_images(found.values()).await {
                return found;
            }
            assert!(Instant::now() < deadline, "the scan found {found:?}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn all_have_images(&self, ids: impl Iterator<Item = &String>) -> bool {
        for id in ids {
            let images: Value = self.json(&format!("/Items/{id}/Images")).await;
            if images.as_array().is_none_or(Vec::is_empty) {
                return false;
            }
        }
        true
    }

    async fn json(&self, path_and_query: &str) -> Value {
        self.client
            .get(format!("{}{path_and_query}", self.base))
            .header("Authorization", &self.auth)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{path_and_query}: {e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("{path_and_query}: {e}"))
    }

    /// `GET`; the status and the body.
    async fn get(&self, path_and_query: &str) -> (u16, Vec<u8>) {
        let response = self
            .client
            .get(format!("{}{path_and_query}", self.base))
            .header("Authorization", &self.auth)
            .timeout(Duration::from_mins(10))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{path_and_query}: {e}"));
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("body").to_vec())
    }
}

fn enabled() -> bool {
    if std::env::var("FERROFIN_CGROUP_TESTS").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipping: set FERROFIN_CGROUP_TESTS=1 to run");
    false
}

/// An image past the resize budget, one over the pixel limit and one over the
/// memory limit are each served as the original file, through the Photo path
/// that reads the file with its EXIF orientation. A smaller resize of the first
/// still runs.
#[tokio::test(flavor = "multi_thread")]
async fn images_past_the_limits_are_served_whole_under_a_cap() {
    if !enabled() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let photos = root.path().join("photos");
    std::fs::create_dir_all(&photos).expect("photos");
    let cases = [
        ("past-budget.png", 13_000, 1, "width=13000&height=12999"),
        ("over-pixels.png", 15_000, 1, "width=15000&height=14999"),
        ("over-memory.png", 14_000, 4, "maxWidth=100"),
    ];
    for (name, side, channels, _) in cases {
        write_png(&photos.join(name), side, side, channels);
    }
    let mut server = CappedServer::boot(root.path(), "1500M", &[]).await;
    let ids = server.add_photos(&photos, cases.len()).await;

    for (name, _, _, query) in cases {
        let (status, body) = server
            .get(&format!("/Items/{}/Images/Primary?{query}", ids[name]))
            .await;
        assert_eq!(status, 200, "{name}");
        assert_eq!(
            body,
            std::fs::read(photos.join(name)).expect("fixture"),
            "{name}: the original file"
        );
        assert!(server.is_running(), "{name}: the server was killed");
    }
    assert_eq!(server.oom_kills(), 0);

    let (status, body) = server
        .get(&format!(
            "/Items/{}/Images/Primary?maxWidth=256",
            ids["past-budget.png"]
        ))
        .await;
    assert_eq!(status, 200);
    assert_ne!(
        body,
        std::fs::read(photos.join("past-budget.png")).expect("fixture"),
        "a thumbnail that fits is still made"
    );
}

/// gl-805: a `blur` draws and writes under the resize budget. At 512 MiB a
/// blurred JPEG of an 8000-pixel square photo fits (7 bytes a pixel and a
/// line of sums, about 428 MiB) and is made; a 12000-pixel one would take about
/// 962 MiB and is served whole. Neither is resized, so the budget is the
/// compositing's alone.
#[tokio::test(flavor = "multi_thread")]
async fn compositing_is_held_against_the_budget_under_a_cap() {
    if !enabled() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let photos = root.path().join("photos");
    std::fs::create_dir_all(&photos).expect("photos");
    write_png(&photos.join("fits.png"), 8_000, 8_000, 1);
    write_png(&photos.join("past.png"), 12_000, 12_000, 1);
    let mut server = CappedServer::boot(
        root.path(),
        "1500M",
        &[("FERROFIN_IMAGE_RESIZE_BUDGET_MB", "512")],
    )
    .await;
    let ids = server.add_photos(&photos, 2).await;

    for (name, drawn) in [("fits.png", true), ("past.png", false)] {
        let (status, body) = server
            .get(&format!(
                "/Items/{}/Images/Primary?blur=3&format=jpg",
                ids[name]
            ))
            .await;
        assert_eq!(status, 200, "{name}");
        let original = std::fs::read(photos.join(name)).expect("fixture");
        if drawn {
            assert!(body.starts_with(&[0xFF, 0xD8, 0xFF]), "{name}: a JPEG");
        } else {
            assert_eq!(body, original, "{name}: the original file");
        }
        assert!(server.is_running(), "{name}: the server was killed");
    }
    assert_eq!(server.oom_kills(), 0);
}

/// Each resize fits the budget and together they would not fit the cap, so
/// sharing the budget is what holds. All of them finish, resized.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_resizes_share_the_budget_under_a_cap() {
    if !enabled() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let photos = root.path().join("photos");
    std::fs::create_dir_all(&photos).expect("photos");
    let source = photos.join("source.png");
    write_png(&source, 4_000, 4_000, 1);
    let mut server = CappedServer::boot(
        root.path(),
        "1500M",
        &[("FERROFIN_IMAGE_RESIZE_BUDGET_MB", "512")],
    )
    .await;
    let ids = server.add_photos(&photos, 1).await;
    let original = std::fs::read(&source).expect("fixture");

    // A different height each, so each is its own cache entry and encode.
    let id = &ids["source.png"];
    let requests = (0..12).map(|index| {
        let server = &server;
        let query = format!(
            "/Items/{id}/Images/Primary?width=4000&height={}",
            3900 + index
        );
        async move { server.get(&query).await }
    });
    let responses = futures_util::future::join_all(requests).await;

    for (status, body) in responses {
        assert_eq!(status, 200);
        assert_ne!(body, original, "served resized");
    }
    assert!(server.is_running(), "the server was killed");
    assert_eq!(server.oom_kills(), 0);
}

/// Bytes in a MiB.
const MIB: u64 = 1024 * 1024;

/// How large the images past the download limit are.
const OVERSIZED: u64 = 200 * MIB;

/// How large the images under the download limit are, and how many there are:
/// together they would not fit the cap if each one were held while it arrived.
const LARGE: u64 = 60 * MIB;
const LARGE_LOGOS: usize = 24;

/// A loopback image source that counts the requests it gets.
struct ImageSource {
    base: String,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ImageSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ImageSource {
    async fn start(logo: Vec<u8>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let base = format!("http://127.0.0.1:{port}");
        let task = tokio::spawn({
            let (hits, base, logo) = (Arc::clone(&hits), base.clone(), Arc::new(logo));
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let (hits, base, logo) = (Arc::clone(&hits), base.clone(), Arc::clone(&logo));
                    tokio::spawn(answer(socket, hits, base, logo));
                }
            }
        });
        Self { base, hits, task }
    }

    fn hits(&self, path: &str) -> usize {
        self.hits
            .lock()
            .expect("hits")
            .get(path)
            .copied()
            .unwrap_or_default()
    }
}

/// Writes `size` bytes starting with the PNG signature, a MiB at a time, each
/// as a chunk when `chunked`. A closed connection ends it: the reader has
/// stopped reading.
async fn write_png_body(socket: &mut tokio::net::TcpStream, size: u64, chunked: bool) {
    use tokio::io::AsyncWriteExt as _;
    let mut block = vec![0_u8; usize::try_from(MIB).expect("block")];
    block[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut left = size;
    while left > 0 {
        let length = usize::try_from(left.min(MIB)).expect("block");
        let written = if chunked {
            let head = format!("{length:x}\r\n");
            socket.write_all(head.as_bytes()).await.is_ok()
                && socket.write_all(&block[..length]).await.is_ok()
                && socket.write_all(b"\r\n").await.is_ok()
        } else {
            socket.write_all(&block[..length]).await.is_ok()
        };
        if !written {
            return;
        }
        block[..8].fill(0);
        left -= length as u64;
    }
    if chunked {
        let _ = socket.write_all(b"0\r\n\r\n").await;
    }
}

async fn answer(
    mut socket: tokio::net::TcpStream,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    base: String,
    logo: Arc<Vec<u8>>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut request = vec![0_u8; 16 * 1024];
    let read = socket.read(&mut request).await.unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    *hits.lock().expect("hits").entry(path.clone()).or_default() += 1;
    let respond = |content_type: &str, body: &[u8]| {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    };
    let response = match path.as_str() {
        "/list.m3u" => {
            let large = (0..LARGE_LOGOS).map(|i| (format!("large{i}"), format!("large-{i}.png")));
            let list = [
                ("declared", "over-declared.png"),
                ("chunked", "over-chunked.png"),
                ("page", "page"),
                ("mislabelled", "mislabelled.png"),
                ("logo", "logo.png"),
            ]
            .map(|(name, logo)| (name.to_owned(), logo.to_owned()))
            .into_iter()
            .chain(large)
            .fold(String::from("#EXTM3U\n"), |mut list, (name, logo)| {
                writeln!(
                    list,
                    "#EXTINF:-1 tvg-id=\"{name}.tv\" tvg-logo=\"{base}/{logo}\",{name}\n\
                     {base}/stream/{name}"
                )
                .expect("a String takes any write");
                list
            });
            respond("audio/x-mpegurl", list.as_bytes())
        }
        large if large.starts_with("/large-") => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {LARGE}\r\n\
                 Connection: close\r\n\r\n"
            );
            if socket.write_all(head.as_bytes()).await.is_ok() {
                write_png_body(&mut socket, LARGE, false).await;
            }
            return;
        }
        "/over-declared.png" => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {OVERSIZED}\r\n\
                 Connection: close\r\n\r\n"
            );
            if socket.write_all(head.as_bytes()).await.is_ok() {
                write_png_body(&mut socket, OVERSIZED, false).await;
            }
            return;
        }
        "/over-chunked.png" => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\
                        Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            if socket.write_all(head.as_bytes()).await.is_ok() {
                write_png_body(&mut socket, OVERSIZED, true).await;
            }
            return;
        }
        "/page" => respond("text/html", b"<html><body>no image here</body></html>"),
        "/mislabelled.png" => respond("image/png", b"<html><body>no image here</body></html>"),
        "/logo.png" => respond("image/png", &logo),
        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    };
    let _ = socket.write_all(&response).await;
    let _ = socket.shutdown().await;
}

impl CappedServer {
    /// Adds `playlist` as an M3U tuner and waits for its `count` channels, each
    /// with a logo row. Returns each one's item id, by channel name.
    async fn add_channels(&self, playlist: &str, count: usize) -> HashMap<String, String> {
        let status = self
            .client
            .post(format!("{}/LiveTv/TunerHosts", self.base))
            .header("Authorization", &self.auth)
            .json(&json!({ "Type": "m3u", "Url": playlist }))
            .send()
            .await
            .expect("add tuner")
            .status();
        assert!(status.is_success(), "add tuner: {status}");

        let deadline = Instant::now() + Duration::from_mins(2);
        loop {
            let channels: Value = self.json("/LiveTv/Channels").await;
            let found: HashMap<String, String> = channels["Items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|channel| {
                    (
                        channel["Name"].as_str().expect("name").to_owned(),
                        channel["Id"].as_str().expect("id").to_owned(),
                    )
                })
                .collect();
            if found.len() == count {
                return found;
            }
            assert!(Instant::now() < deadline, "the guide found {found:?}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// `GET`, the status alone, or 0 for no answer.
    async fn status_of(&self, path: &str) -> u16 {
        self.client
            .get(format!("{}{path}", self.base))
            .timeout(Duration::from_mins(5))
            .send()
            .await
            .map_or(0, |response| response.status().as_u16())
    }

    /// `memory.peak` of the cgroup, in MiB.
    fn memory_peak_mib(&self) -> u64 {
        // The kernel removes a killed server's cgroup with it: no reading.
        std::fs::read_to_string(self.cgroup.join("memory.peak"))
            .ok()
            .and_then(|peak| peak.trim().parse::<u64>().ok())
            .unwrap_or_default()
            / MIB
    }
}

/// Remote images stay within the download limit, whether a body's length is
/// declared or found out by reading, with one fetch for all the requests for
/// one URL. A URL that gave no image is asked once, and real logos come
/// through, one or many.
#[tokio::test(flavor = "multi_thread")]
async fn remote_images_are_bounded_under_a_cap() {
    if !enabled() {
        return;
    }
    // On disk: the cache holds what the images weigh, and a `/tmp` on tmpfs
    // would charge those files to the cgroup, with no swap to page them out to.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let logo_path = root.path().join("logo.png");
    write_png(&logo_path, 16, 16, 4);
    let logo = std::fs::read(&logo_path).expect("fixture");
    let source = ImageSource::start(logo.clone()).await;

    let mut server = CappedServer::boot(root.path(), "1500M", &[]).await;
    let channels = server
        .add_channels(&format!("{}/list.m3u", source.base), 5 + LARGE_LOGOS)
        .await;

    for (name, path) in [
        ("declared", "/over-declared.png"),
        ("chunked", "/over-chunked.png"),
    ] {
        let url = format!("/Items/{}/Images/Primary", channels[name]);
        let requests = (0..8).map(|_| server.status_of(&url));
        let statuses = futures_util::future::join_all(requests).await;
        eprintln!(
            "{name}: {statuses:?}, {} fetches, memory peak {} MiB",
            source.hits(path),
            server.memory_peak_mib()
        );
        assert!(server.is_running(), "{name}: the server was killed");
        assert_eq!(server.oom_kills(), 0, "{name}");
        assert_eq!(statuses, [404; 8], "{name}");
        assert_eq!(source.hits(path), 1, "{name}: one fetch for all of them");
        assert_eq!(server.status_of(&url).await, 404, "{name}");
        assert_eq!(source.hits(path), 1, "{name}: the refusal is remembered");
    }

    for (name, path) in [("page", "/page"), ("mislabelled", "/mislabelled.png")] {
        let url = format!("/Items/{}/Images/Primary", channels[name]);
        for _ in 0..5 {
            assert_eq!(server.status_of(&url).await, 404, "{name}");
        }
        eprintln!("{name}: {} fetches", source.hits(path));
        assert_eq!(source.hits(path), 1, "{name}: the refusal is remembered");
    }

    let (status, body) = server
        .get(&format!("/Items/{}/Images/Primary", channels["logo"]))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, logo, "a real logo is served");

    let shared = &server;
    let requests = (0..LARGE_LOGOS).map(|index| {
        let url = format!(
            "/Items/{}/Images/Primary",
            channels[&format!("large{index}")]
        );
        async move { shared.status_of(&url).await }
    });
    let statuses = futures_util::future::join_all(requests).await;
    eprintln!(
        "large: {statuses:?}, memory peak {} MiB",
        server.memory_peak_mib()
    );
    assert!(server.is_running(), "large: the server was killed");
    assert_eq!(server.oom_kills(), 0, "large");
    assert_eq!(statuses, [200; LARGE_LOGOS], "large");
}
