//! `library_roots` (`FERROFIN_LIBRARY_ROOTS`, or `library_roots` in
//! `config.toml`): a fresh install starts with one library per configured root
//! and scans them, and a later boot of the same server adds none, so a library
//! its administrator removed stays removed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferrofin_server::config::Config;

#[path = "support/boot.rs"]
mod boot;

use boot::{is_up, spawn_server};

const ADMIN_USER: &str = "seed-admin";
const ADMIN_PASSWORD: &str = "seed-pw";
const CLIENT: &str = r#"MediaBrowser Client="library-roots-test", Device="test", DeviceId="library-roots-test", Version="1""#;

/// Ceiling for a single request, here only to turn a hang into a failure:
/// `/Users/AuthenticateByName` runs Jellyfin's PBKDF2 in an unoptimized build,
/// which takes seconds on a busy machine (see `restart.rs`).
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the seeded libraries' first scan gets to index two files.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

fn seed_config(root: &Path, port: u16, library_roots: &[PathBuf]) -> Config {
    Config {
        server_name: "ferrofin-library-roots".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        library_roots: library_roots.to_vec(),
        port,
        ..Config::test_stub(root)
    }
}

async fn token(client: &reqwest::Client, base: &str) -> String {
    let body: serde_json::Value = client
        .post(format!("{base}/Users/AuthenticateByName"))
        .header("Authorization", CLIENT)
        .json(&serde_json::json!({"Username": ADMIN_USER, "Pw": ADMIN_PASSWORD}))
        .send()
        .await
        .expect("auth request")
        .json()
        .await
        .expect("auth json");
    body["AccessToken"]
        .as_str()
        .expect("access token")
        .to_owned()
}

async fn send(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
) -> reqwest::Response {
    client
        .request(method, url)
        .header("Authorization", format!("{CLIENT}, Token=\"{token}\""))
        .send()
        .await
        .expect("request")
}

/// `(Name, Locations, CollectionType)` of each library
/// `GET /Library/VirtualFolders` lists, sorted by name.
async fn libraries(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Vec<(String, Vec<String>, serde_json::Value)> {
    let response = send(
        client,
        reqwest::Method::GET,
        format!("{base}/Library/VirtualFolders"),
        token,
    )
    .await;
    assert_eq!(response.status(), 200);
    let folders: serde_json::Value = response.json().await.expect("folders json");
    let mut rows: Vec<_> = folders
        .as_array()
        .expect("a list of libraries")
        .iter()
        .map(|folder| {
            (
                folder["Name"].as_str().expect("name").to_owned(),
                folder["Locations"]
                    .as_array()
                    .expect("locations")
                    .iter()
                    .map(|location| location.as_str().expect("location").to_owned())
                    .collect(),
                folder["CollectionType"].clone(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// How many movies and videos the server has indexed.
async fn video_count(client: &reqwest::Client, base: &str, token: &str) -> Option<i64> {
    let body: serde_json::Value = send(
        client,
        reqwest::Method::GET,
        format!("{base}/Items?recursive=true&includeItemTypes=Movie,Video&limit=0"),
        token,
    )
    .await
    .json()
    .await
    .ok()?;
    body["TotalRecordCount"].as_i64()
}

/// `POST /System/Shutdown`, then waits for `run` to return.
async fn shut_down(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    server: std::thread::JoinHandle<anyhow::Result<()>>,
) {
    let status = send(
        client,
        reqwest::Method::POST,
        format!("{base}/System/Shutdown"),
        token,
    )
    .await
    .status();
    assert_eq!(status, 204);
    tokio::time::timeout(
        Duration::from_mins(1),
        tokio::task::spawn_blocking(move || server.join()),
    )
    .await
    .expect("run returns after shutdown")
    .expect("join task")
    .expect("server thread did not panic")
    .expect("run exits cleanly");
    assert!(!is_up(client, base, name).await, "the server stopped");
}

fn location(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp path").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_install_seeds_one_library_per_root_and_a_later_boot_adds_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("client");
    let media = tmp.path().join("media");
    // Two roots with the same folder name: the second library is `Movies2`.
    let movies = media.join("a").join("Movies");
    let more_movies = media.join("b").join("Movies");
    for (root, title) in [
        (&movies, "First Film (2001)"),
        (&more_movies, "Second Film (2002)"),
    ] {
        let folder = root.join(title);
        std::fs::create_dir_all(&folder).expect("movie folder");
        std::fs::write(folder.join(format!("{title}.mkv")), [0u8; 1024]).expect("movie file");
    }
    // Neither of these can be a library: each is skipped with a warning.
    let missing = media.join("missing");
    let not_a_folder = media.join("notes.txt");
    std::fs::write(&not_a_folder, "not a library").expect("plain file");
    let roots = [movies.clone(), missing, not_a_folder, more_movies.clone()];

    let (server, base, config) =
        spawn_server(&client, |port| seed_config(tmp.path(), port, &roots)).await;
    let tok = token(&client, &base).await;
    assert_eq!(
        libraries(&client, &base, &tok).await,
        [
            (
                "Movies".to_owned(),
                vec![location(&movies)],
                serde_json::Value::Null
            ),
            (
                "Movies2".to_owned(),
                vec![location(&more_movies)],
                serde_json::Value::Null
            ),
        ],
        "one mixed-content library per existing root, named after its folder"
    );
    // The first scan was queued: both films are indexed without anyone asking.
    let deadline = Instant::now() + SCAN_DEADLINE;
    loop {
        let count = video_count(&client, &base, &tok).await;
        if count == Some(2) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the seeded libraries were not scanned: {count:?} videos indexed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The administrator removes one of them.
    let status = send(
        &client,
        reqwest::Method::DELETE,
        format!("{base}/Library/VirtualFolders?name=Movies2"),
        &tok,
    )
    .await
    .status();
    assert_eq!(status, 204);
    shut_down(&client, &base, &tok, &config.server_name, server).await;

    // The same server boots again with another root configured: the install is
    // no longer fresh, so nothing is seeded and the removed library stays gone.
    let shows = media.join("Shows");
    std::fs::create_dir_all(&shows).expect("shows folder");
    let roots_later = [movies.clone(), more_movies, shows];
    let (server, base, config) =
        spawn_server(&client, |port| seed_config(tmp.path(), port, &roots_later)).await;
    let tok = token(&client, &base).await;
    assert_eq!(
        libraries(&client, &base, &tok).await,
        [(
            "Movies".to_owned(),
            vec![location(&movies)],
            serde_json::Value::Null
        )],
        "a later boot seeds nothing"
    );
    shut_down(&client, &base, &tok, &config.server_name, server).await;
}
