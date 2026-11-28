//! A Live TV configuration write runs the "Refresh Guide" task: the real
//! server over HTTP, a tuner on loopback the test can hold, and requests sent
//! at the same moment.
//!
//! Upstream ends each such write with
//! `CancelIfRunningAndQueue<RefreshGuideScheduledTask>()`. The dashboard then
//! shows the refresh as a run of the task, with its progress; a write that
//! arrives while one runs stops it, and a new run starts after it; and the
//! guide comes to rest on the last write.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ferrofin_server::config::Config;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[path = "support/boot.rs"]
mod boot;

const ADMIN_PASSWORD: &str = "guide-refresh-task-pw";
/// How long anything here may take.
const DEADLINE: Duration = Duration::from_mins(1);
/// How long the tuner takes over a playlist, so that requests sent together
/// are all at work together.
const PLAYLIST_DELAY: Duration = Duration::from_millis(100);
/// The guide's channels besides `one`, each airing one show named after it.
const OTHERS: [&str; 8] = [
    "alt1", "alt2", "alt3", "alt4", "alt5", "alt6", "alt7", "alt8",
];

/// A one-channel M3U tuner on loopback.
struct Tuner {
    playlist: String,
    /// How many more playlist requests are answered while the tuner is held.
    unheld: Arc<AtomicUsize>,
    /// Whether playlist requests wait, once `unheld` is used up.
    held: tokio::sync::watch::Sender<bool>,
}

impl Tuner {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tuner");
        let playlist = format!(
            "http://{}/playlist.m3u",
            listener.local_addr().expect("local addr")
        );
        let unheld = Arc::new(AtomicUsize::new(0));
        let (held, _) = tokio::sync::watch::channel(false);
        let (left, waits) = (Arc::clone(&unheld), held.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(answer(socket, Arc::clone(&left), waits.subscribe()));
            }
        });
        Self {
            playlist,
            unheld,
            held,
        }
    }

    /// Holds every playlist request after the next `unheld`, until
    /// [`release`](Self::release).
    fn hold_after(&self, unheld: usize) {
        self.unheld.store(unheld, Ordering::SeqCst);
        self.held.send_replace(true);
    }

    fn release(&self) {
        self.held.send_replace(false);
    }
}

async fn answer(
    mut socket: tokio::net::TcpStream,
    unheld: Arc<AtomicUsize>,
    mut held: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut request = vec![0; 4096];
    let _ = socket.read(&mut request).await?;
    let passes = unheld
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_ok();
    if !passes {
        let _ = held.wait_for(|held| !held).await;
    }
    tokio::time::sleep(PLAYLIST_DELAY).await;
    let playlist =
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",One\nhttp://tuner.invalid/one\n";
    let status_and_headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        playlist.len()
    );
    socket
        .write_all((status_and_headers + playlist).as_bytes())
        .await
}

/// The administrator of the server at `base`.
#[derive(Clone)]
struct Admin {
    client: reqwest::Client,
    base: String,
    authorization: String,
}

impl Admin {
    async fn sign_in(client: &reqwest::Client, base: &str) -> Self {
        let device = r#"Client="test", Device="d", DeviceId="guide-refresh-task", Version="1""#;
        let session: Value = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", format!("MediaBrowser {device}"))
            .json(&json!({ "Username": "admin", "Pw": ADMIN_PASSWORD }))
            .send()
            .await
            .expect("sign in")
            .json()
            .await
            .expect("session");
        let token = session["AccessToken"].as_str().expect("token");
        Self {
            client: client.clone(),
            base: base.to_owned(),
            authorization: format!(r#"MediaBrowser Token="{token}", {device}"#),
        }
    }

    /// The JSON of a `200` to `GET uri`.
    async fn get(&self, uri: &str) -> Value {
        let response = self
            .client
            .get(format!("{}{uri}", self.base))
            .header("Authorization", &self.authorization)
            .send()
            .await
            .expect("get");
        assert!(
            response.status().is_success(),
            "GET {uri}: {}",
            response.status()
        );
        response.json().await.expect("json")
    }

    /// The JSON of a `200` to `POST uri` with `body`.
    async fn post(&self, uri: &str, body: &Value) -> Value {
        let response = self
            .client
            .post(format!("{}{uri}", self.base))
            .header("Authorization", &self.authorization)
            .json(body)
            .send()
            .await
            .expect("post");
        let status = response.status();
        let text = response.text().await.expect("body");
        assert!(status.is_success(), "POST {uri}: {status}: {text}");
        serde_json::from_str(&text).expect("json")
    }

    /// The "Refresh Guide" task as `GET /ScheduledTasks` lists it, once
    /// `wanted` holds of it.
    async fn guide_task_when(&self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let tasks = self.get("/ScheduledTasks").await;
            let task = tasks
                .as_array()
                .expect("tasks")
                .iter()
                .find(|task| task["Key"] == "RefreshGuide")
                .expect("the Refresh Guide task")
                .clone();
            if wanted(&task) {
                return task;
            }
            assert!(Instant::now() < deadline, "never {what}: {task}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The names `GET uri` lists, sorted.
    async fn names(&self, uri: &str) -> Vec<String> {
        let mut names: Vec<String> = self.get(uri).await["Items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["Name"].as_str().expect("name").to_owned())
            .collect();
        names.sort();
        names
    }
}

/// The server, its administrator and a tuner; the guide file is written.
struct Fixture {
    admin: Admin,
    tuner: Tuner,
    guide: std::path::PathBuf,
    _temp: tempfile::TempDir,
    _server: std::thread::JoinHandle<anyhow::Result<()>>,
}

impl Fixture {
    async fn save_tuner(&self) {
        let body = json!({ "Type": "m3u", "Url": self.tuner.playlist });
        self.admin.post("/LiveTv/TunerHosts", &body).await;
    }

    /// Saves the guide as a listings provider, and returns the provider's id.
    async fn save_guide(&self) -> String {
        let body = json!({
            "Type": "xmltv",
            "Path": self.guide.to_string_lossy(),
            "EnableAllTuners": true,
        });
        let saved = self.admin.post("/LiveTv/ListingProviders", &body).await;
        saved["Id"].as_str().expect("provider id").to_owned()
    }
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for dir in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(dir)).expect("dir");
    }
    let day = (chrono::Utc::now() + chrono::Duration::days(1)).format("%Y%m%d");
    let mut guide = String::from("<tv>");
    for channel in std::iter::once("one").chain(OTHERS) {
        write!(
            guide,
            "<channel id=\"{channel}\"><display-name>{channel}</display-name></channel>\
             <programme start=\"{day}060000 +0000\" stop=\"{day}070000 +0000\" \
             channel=\"{channel}\"><title>show {channel}</title></programme>"
        )
        .expect("guide");
    }
    let guide_file = temp.path().join("guide.xml");
    std::fs::write(&guide_file, guide + "</tv>").expect("guide");

    let client = reqwest::Client::builder()
        .timeout(DEADLINE)
        .build()
        .expect("client");
    let root = temp.path().to_owned();
    let (server, base, _) = boot::spawn_server(&client, move |port| Config {
        server_name: "ferrofin-guide-refresh-task".to_owned(),
        admin_user: "admin".to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        ..Config::test_stub(&root)
    })
    .await;
    Fixture {
        admin: Admin::sign_in(&client, &base).await,
        tuner: Tuner::start().await,
        guide: guide_file,
        _temp: temp,
        _server: server,
    }
}

/// When a task result started, as the server wrote it.
fn started(result: &Value) -> chrono::DateTime<chrono::Utc> {
    result["StartTimeUtc"]
        .as_str()
        .expect("StartTimeUtc")
        .parse()
        .expect("a time")
}

/// The refresh a tuner save queues is a run of the task: Running with the
/// refresh's own progress while it reads the tuner, then Completed.
#[tokio::test(flavor = "multi_thread")]
async fn a_tuner_save_runs_the_refresh_guide_task() {
    let f = fixture().await;
    let admin = &f.admin;
    // The save reads the playlist itself to validate it; the refresh's own
    // read is held.
    f.tuner.hold_after(1);
    f.save_tuner().await;

    // 10 is what the refresh reports before it reads a tuner.
    admin
        .guide_task_when("Running at 10 %", |task| {
            task["State"] == "Running" && task["CurrentProgressPercentage"] == 10.0
        })
        .await;
    assert_eq!(admin.names("/LiveTv/Channels").await, Vec::<String>::new());

    f.tuner.release();
    let idle = admin
        .guide_task_when("Idle", |task| task["State"] == "Idle")
        .await;
    assert_eq!(idle["LastExecutionResult"]["Status"], "Completed", "{idle}");
    assert_eq!(admin.names("/LiveTv/Channels").await, ["One"]);
}

/// A write that arrives while a refresh runs stops it, and the refresh that
/// starts after the write reads what it stored.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_during_a_refresh_stops_it_and_the_next_refresh_reads_the_write() {
    let f = fixture().await;
    let admin = &f.admin;
    f.tuner.hold_after(1);
    f.save_tuner().await;
    admin
        .guide_task_when("Running", |task| task["State"] == "Running")
        .await;

    // The tuner's refresh is still reading the playlist. The guide is saved.
    let before_the_write = chrono::Utc::now();
    f.save_guide().await;
    // That run was stopped, and one that started after the write is under
    // way: it is held at the tuner too.
    let second = admin
        .guide_task_when("running again after a cancelled run", |task| {
            task["State"] == "Running" && task["LastExecutionResult"]["Status"] == "Cancelled"
        })
        .await;
    assert!(
        started(&second["LastExecutionResult"]) < before_the_write,
        "the stopped run is the tuner save's: {second}"
    );
    assert_eq!(admin.names("/LiveTv/Programs").await, Vec::<String>::new());

    f.tuner.release();
    let idle = admin
        .guide_task_when("Idle", |task| task["State"] == "Idle")
        .await;
    let last = &idle["LastExecutionResult"];
    assert_eq!(last["Status"], "Completed", "{idle}");
    assert!(started(last) >= before_the_write, "{idle}");
    // It read the guide the write stored.
    assert_eq!(admin.names("/LiveTv/Programs").await, ["show one"]);
}

/// Mappings sent at the same moment each queue the task, and the guide comes
/// to rest on the mapping that was stored last.
#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_mappings_leave_the_guide_on_the_one_stored_last() {
    let f = fixture().await;
    let admin = &f.admin;
    f.save_tuner().await;
    let provider = f.save_guide().await;
    admin
        .guide_task_when("at rest on the unmapped guide", |task| {
            task["State"] == "Idle" && task["LastExecutionResult"]["Status"] == "Completed"
        })
        .await;
    let options = format!("/LiveTv/ChannelMappingOptions?providerId={provider}");
    let tuner_channel = admin.get(&options).await["TunerChannels"][0]["Id"]
        .as_str()
        .expect("tuner channel id")
        .to_owned();

    // Every request maps the one tuner channel, each to another guide
    // channel. Each reads the playlist, so they are all at work at once.
    let posts = OTHERS.map(|channel| {
        let body = json!({
            "ProviderId": provider,
            "TunerChannelId": tuner_channel,
            "ProviderChannelId": channel,
        });
        let admin = admin.clone();
        async move { admin.post("/LiveTv/ChannelMappings", &body).await }
    });
    futures_util::future::join_all(posts).await;

    let stored = admin.get(&options).await["Mappings"].clone();
    let stored = stored.as_array().expect("mappings");
    assert_eq!(stored.len(), 1, "one pair for the one channel: {stored:?}");
    let show = format!("show {}", stored[0]["Value"].as_str().expect("value"));
    let deadline = Instant::now() + DEADLINE;
    loop {
        let task = admin.guide_task_when("listed", |_| true).await;
        let shows = admin.names("/LiveTv/Programs").await;
        if task["State"] == "Idle"
            && task["LastExecutionResult"]["Status"] == "Completed"
            && shows == [show.clone()]
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the guide never came to rest on {show}: it lists {shows:?}, and the task is {task}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
