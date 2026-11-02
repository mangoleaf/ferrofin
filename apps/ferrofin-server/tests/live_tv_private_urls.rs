//! A tuner's upstream URL, end to end: the real server, a real M3U tuner on a
//! loopback provider, and the requests jellyfin-web makes.
//!
//! IPTV stream URLs usually carry the account's credentials, so a caller who is
//! not an administrator gets neither the URL nor direct play. What matters is
//! that playback survives that: with the `Path` gone a client has to stream
//! through `/Videos/{id}/stream`, which the server relays from the source it
//! holds. The handler tests stop at the JSON; this one follows the URL
//! `playbackmanager.js` builds and reads the provider's bytes back through it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_server::config::Config;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[path = "support/boot.rs"]
mod boot;

const ADMIN_PASSWORD: &str = "live-tv-private-urls-pw";
const VIEWER_PASSWORD: &str = "viewer-pw";
/// What marks the provider's account in a URL.
const SECRET: &str = "acct-secret";
/// A tuner's `User-Agent`, which the relay must send on the client's behalf.
const TUNER_AGENT: &str = "tuner-agent";
/// What the provider serves for a channel.
const PAYLOAD: &[u8] = b"the provider's own bytes";
/// How long a lineup may take to appear, or a relayed stream to start.
const DEADLINE: Duration = Duration::from_secs(30);
/// Serves a playlist of one channel on loopback, and records the `User-Agent`
/// of each stream request. The channel is an MP4 URL, which is never shared
/// and so stays a pass-through: the opened source still holds the provider's
/// URL. (A buffered stream needs real MPEG-TS, or ffprobe waits on it for ever;
/// the handler tests cover its response.)
async fn provider() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let origin = format!("http://{}", listener.local_addr().expect("local addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (base, recorded) = (origin.clone(), Arc::clone(&seen));
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(answer(socket, base.clone(), Arc::clone(&recorded)));
        }
    });
    (format!("{origin}/playlist.m3u"), seen)
}

async fn answer(
    mut socket: tokio::net::TcpStream,
    origin: String,
    seen: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut request = vec![0; 4096];
    let read = socket.read(&mut request).await?;
    let request = String::from_utf8_lossy(&request[..read]).to_string();
    let path = request.split_whitespace().nth(1).unwrap_or_default();
    if path == "/playlist.m3u" {
        let playlist = format!(
            "#EXTM3U\n\
             #EXTINF:-1 tvg-id=\"pass\" tvg-chno=\"1\",Pass\n{origin}/live/{SECRET}/1.mp4\n"
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            playlist.len()
        );
        return socket.write_all((head + &playlist).as_bytes()).await;
    }
    if let Some(agent) = request
        .lines()
        .find_map(|line| line.strip_prefix("user-agent: "))
    {
        seen.lock().expect("lock").push(agent.to_owned());
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        PAYLOAD.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(PAYLOAD).await
}

/// One signed-in caller of the server at `base`.
struct Caller {
    client: reqwest::Client,
    base: String,
    token: String,
    user_id: String,
}

impl Caller {
    async fn sign_in(client: &reqwest::Client, base: &str, name: &str, password: &str) -> Self {
        let session: Value = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", authorization(None))
            .json(&json!({ "Username": name, "Pw": password }))
            .send()
            .await
            .expect("sign in")
            .json()
            .await
            .expect("session");
        Self {
            client: client.clone(),
            base: base.to_owned(),
            token: session["AccessToken"].as_str().expect("token").to_owned(),
            user_id: session["User"]["Id"].as_str().expect("id").to_owned(),
        }
    }

    fn request(&self, method: reqwest::Method, uri: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{uri}", self.base))
            .header("Authorization", authorization(Some(&self.token)))
    }

    /// `POST` `body` to `uri`, returning the raw text of the `200`.
    async fn post(&self, uri: &str, body: &Value) -> String {
        let response = self
            .request(reqwest::Method::POST, uri)
            .json(body)
            .send()
            .await
            .expect("post");
        let status = response.status();
        let text = response.text().await.expect("body");
        assert!(status.is_success(), "POST {uri}: {status}: {text}");
        text
    }

    /// The channels' ids by name, once the tuner's lineup is listed.
    async fn channels(&self) -> Vec<(String, String)> {
        let deadline = std::time::Instant::now() + DEADLINE;
        loop {
            let listed: Value = self
                .request(reqwest::Method::GET, "/LiveTv/Channels")
                .send()
                .await
                .expect("channels")
                .json()
                .await
                .expect("channels json");
            let items = listed["Items"].as_array().cloned().unwrap_or_default();
            if !items.is_empty() {
                return items
                    .iter()
                    .map(|item| {
                        (
                            item["Name"].as_str().expect("name").to_owned(),
                            item["Id"].as_str().expect("id").to_owned(),
                        )
                    })
                    .collect();
            }
            assert!(std::time::Instant::now() < deadline, "no lineup appeared");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `POST PlaybackInfo` as jellyfin-web sends it to play, opening the
    /// stream: the source, and the raw response.
    async fn play(&self, channel: &str) -> (Value, String) {
        let body = self
            .post(
                &format!("/Items/{channel}/PlaybackInfo?UserId={}", self.user_id),
                &json!({ "UserId": self.user_id, "IsPlayback": true, "AutoOpenLiveStream": true }),
            )
            .await;
        let parsed: Value = serde_json::from_str(&body).expect("playback info");
        (parsed["MediaSources"][0].clone(), body)
    }

    /// The direct-stream URL `createStreamInfo` in `playbackmanager.js` builds
    /// for a source it may not direct play.
    fn stream_url(&self, channel: &str, source: &Value, container: &str) -> String {
        format!(
            "{}/Videos/{channel}/stream.{container}?Static=true&mediaSourceId={}&deviceId=web\
             &ApiKey={}&LiveStreamId={}",
            self.base,
            source["Id"].as_str().expect("source id"),
            self.token,
            source["LiveStreamId"].as_str().expect("live stream id"),
        )
    }
}

fn authorization(token: Option<&str>) -> String {
    let client = r#"Client="test", Device="d", DeviceId="live-tv-private-urls", Version="1""#;
    match token {
        Some(token) => format!(r#"MediaBrowser Token="{token}", {client}"#),
        None => format!("MediaBrowser {client}"),
    }
}

/// The server, its tuner, an administrator and an ordinary account.
struct Fixture {
    admin: Caller,
    viewer: Caller,
    channels: Vec<(String, String)>,
    agents: Arc<Mutex<Vec<String>>>,
    _temp: tempfile::TempDir,
    _server: std::thread::JoinHandle<anyhow::Result<()>>,
}

impl Fixture {
    fn channel(&self, name: &str) -> &str {
        let found = self.channels.iter().find(|(n, _)| n == name);
        &found.expect("channel").1
    }
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for dir in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(dir)).expect("dir");
    }
    let (playlist, agents) = provider().await;
    let client = reqwest::Client::builder()
        .timeout(DEADLINE)
        .build()
        .expect("client");
    let root = temp.path().to_owned();
    let (server, base, _) = boot::spawn_server(&client, move |port| Config {
        server_name: "ferrofin-live-tv-private-urls".to_owned(),
        admin_user: "admin".to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        ..Config::test_stub(&root)
    })
    .await;
    let admin = Caller::sign_in(&client, &base, "admin", ADMIN_PASSWORD).await;
    admin
        .post(
            "/Users/New",
            &json!({ "Name": "viewer", "Password": VIEWER_PASSWORD }),
        )
        .await;
    admin
        .post(
            "/LiveTv/TunerHosts",
            &json!({ "Type": "m3u", "Url": playlist, "UserAgent": TUNER_AGENT }),
        )
        .await;
    let viewer = Caller::sign_in(&client, &base, "viewer", VIEWER_PASSWORD).await;
    let channels = viewer.channels().await;
    Fixture {
        admin,
        viewer,
        channels,
        agents,
        _temp: temp,
        _server: server,
    }
}

/// An ordinary account gets a blank `Path` and no direct play on every route,
/// opened or not, and no response names the provider's URL. The stream they
/// then ask for is the provider's, relayed by the server.
/// An administrator keeps the URL, and so does an API key, the administrator
/// role's other holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_admin_gets_no_tuner_url_and_plays_through_the_relay() {
    let fixture = fixture().await;
    let viewer = &fixture.viewer;
    let channel = fixture.channel("Pass");

    let info = |caller: &Caller| format!("/Items/{channel}/PlaybackInfo?UserId={}", caller.user_id);
    let mut open_token = Value::Null;
    for response in [
        viewer.post(&info(viewer), &json!({})).await,
        viewer
            .request(reqwest::Method::GET, &info(viewer))
            .send()
            .await
            .expect("get playback info")
            .text()
            .await
            .expect("body"),
    ] {
        assert!(!response.contains(SECRET), "{response}");
        let source = &serde_json::from_str::<Value>(&response).expect("json")["MediaSources"][0];
        assert_eq!(source["Path"], "", "{source}");
        assert_eq!(source["SupportsDirectPlay"], false, "{source}");
        assert_eq!(source["SupportsDirectStream"], true, "{source}");
        open_token = source["OpenToken"].clone();
    }

    // The pass-through stream: nothing buffers it, so the server holds the
    // provider's URL and the opened source used to hand it out.
    let (source, response) = viewer.play(channel).await;
    assert!(!response.contains(SECRET), "{response}");
    assert_eq!(source["Path"], "", "{source}");
    assert_eq!(source["SupportsDirectPlay"], false, "{source}");
    let relayed = viewer
        .client
        .get(viewer.stream_url(channel, &source, "mp4"))
        .send()
        .await
        .expect("stream");
    assert_eq!(relayed.status(), 200);
    assert_eq!(&relayed.bytes().await.expect("bytes")[..], PAYLOAD);
    assert!(
        fixture
            .agents
            .lock()
            .expect("lock")
            .iter()
            .any(|agent| agent == TUNER_AGENT),
        "the relay reads the provider as the tuner does"
    );

    // `POST /LiveStreams/Open`, which jellyfin-web uses for a source
    // PlaybackInfo did not open.
    let opened = viewer
        .post(
            &format!(
                "/LiveStreams/Open?UserId={}&ItemId={channel}",
                viewer.user_id
            ),
            &json!({ "OpenToken": open_token }),
        )
        .await;
    assert!(!opened.contains(SECRET), "{opened}");
    let opened: Value = serde_json::from_str(&opened).expect("json");
    assert_eq!(opened["MediaSource"]["Path"], "", "{opened}");
    assert_eq!(opened["MediaSource"]["SupportsDirectPlay"], false);

    // An administrator keeps the URL, unopened and opened alike.
    let admin = &fixture.admin;
    let unopened = admin.post(&info(admin), &json!({})).await;
    assert!(unopened.contains(SECRET), "{unopened}");
    let (source, _) = admin.play(channel).await;
    assert!(source["Path"].as_str().expect("path").contains(SECRET));
    assert_eq!(source["SupportsDirectPlay"], true, "{source}");

    // An API key is an administrator, whichever user it names.
    admin.post("/Auth/Keys?App=private-urls", &json!({})).await;
    let keys: Value = admin
        .request(reqwest::Method::GET, "/Auth/Keys")
        .send()
        .await
        .expect("keys")
        .json()
        .await
        .expect("keys json");
    let key = keys["Items"][0]["AccessToken"].as_str().expect("key");
    let by_key = Caller {
        client: admin.client.clone(),
        base: admin.base.clone(),
        token: key.to_owned(),
        user_id: viewer.user_id.clone(),
    };
    let response = by_key.post(&info(viewer), &json!({})).await;
    assert!(response.contains(SECRET), "{response}");
}
