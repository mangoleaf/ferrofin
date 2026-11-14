//! A box set's TMDb collection search, end to end over real HTTP: a box set
//! with no Tmdb id looks itself up by `ParseName`, then `TmdbUtils.CleanName`
//! (`TmdbBoxSetProvider.GetMetadata`, `TmdbBoxSetProvider.cs:102-109`), and the
//! hit fills it in.
//!
//! Boots the real server ([`ferrofin_server::run`]) with TMDb answered by an
//! in-process stand-in through [`Config::provider_endpoints`]; every other
//! provider stays on the offline address. `POST /Collections` queues the
//! refresh a client's "New Collection" dialog relies on.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::{Json, Router, http::Uri};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="box-set-search-test", Device="fixture", DeviceId="box-set-search-test", Version="1""#;

/// How long the queued refresh gets to fill the box set in.
const REFRESH_DEADLINE: Duration = Duration::from_mins(1);

/// The overview the stand-in's one collection carries.
const OVERVIEW: &str = "The boy who lived.";

/// The `query` of each collection search the stand-in has answered.
type Searches = Arc<Mutex<Vec<String>>>;

/// A TMDb stand-in whose every collection search finds collection 1241.
async fn tmdb(State(searches): State<Searches>, uri: Uri) -> Json<Value> {
    Json(match uri.path() {
        "/search/collection" => {
            let url = reqwest::Url::parse(&format!("http://stand-in{uri}")).unwrap();
            let query = url.query_pairs().find(|(key, _)| key == "query").unwrap();
            searches.lock().unwrap().push(query.1.into_owned());
            json!({"results":[{"id":1241,"name":"Harry Potter Collection"}]})
        }
        "/collection/1241" => {
            json!({"id":1241,"name":"Harry Potter Collection","overview":OVERVIEW})
        }
        _ => json!({}),
    })
}

struct Api {
    client: reqwest::Client,
    base: String,
    auth: String,
}

impl Api {
    /// `POST /Collections?name=…`: the new box set's id.
    async fn create_collection(&self, name: &str) -> String {
        let created: Value = self
            .client
            .post(format!("{}/Collections", self.base))
            .header("Authorization", &self.auth)
            .query(&[("name", name)])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        created["Id"].as_str().unwrap().to_owned()
    }

    /// The box set's overview, once its queued refresh has saved one.
    async fn await_overview(&self, id: &str) -> String {
        let deadline = Instant::now() + REFRESH_DEADLINE;
        loop {
            let item: Value = self
                .client
                .get(format!("{}/Items/{id}?Fields=Overview", self.base))
                .header("Authorization", &self.auth)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if let Some(overview) = item["Overview"].as_str() {
                return overview.to_owned();
            }
            assert!(Instant::now() < deadline, "never refreshed: {item}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_collection_searches_tmdb_by_its_parsed_and_cleaned_name() {
    let tmp = tempfile::tempdir().unwrap();
    let searches = Searches::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn({
        let router = Router::new().fallback(tmdb).with_state(searches.clone());
        async move { axum::serve(listener, router).await.unwrap() }
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let (server, base, _config) = boot::spawn_server(&client, |port| Config {
        port,
        provider_endpoints: ProviderEndpoints {
            tmdb: Some(endpoint.clone()),
            ..ProviderEndpoints::offline()
        },
        ..Config::test_stub(tmp.path())
    })
    .await;
    let login: Value = client
        .post(format!("{base}/Users/AuthenticateByName"))
        .header("Authorization", CLIENT)
        .json(&json!({"Username":"admin","Pw":""}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let auth = format!(
        "{CLIENT}, Token=\"{}\"",
        login["AccessToken"].as_str().unwrap()
    );
    let api = Api { client, base, auth };

    let names = [
        "Harry Potter (2001)",
        "Harry Potter (2001-2011)",
        "Alien 4K",
    ];
    for name in names {
        let id = api.create_collection(name).await;
        assert_eq!(api.await_overview(&id).await, OVERVIEW, "{name}");
    }

    // What was searched, not how often: the first create queues a scan of the
    // new Collections library once its own refresh has run, and that scan's
    // box-set pass can refresh a later box set whose queued refresh has not
    // saved the id yet.
    let seen = searches.lock().unwrap().clone();
    // `CleanDateTimes` is greedy: of two years only the last comes out, so the
    // range keeps its first, as it does in Jellyfin.
    for query in ["Harry Potter", "Harry Potter 2001", "Alien"] {
        assert!(seen.iter().any(|s| s == query), "{query}: {seen:?}");
    }
    for name in names {
        assert!(!seen.iter().any(|s| s == name), "{name}: {seen:?}");
    }

    let shutdown = api
        .client
        .post(format!("{}/System/Shutdown", api.base))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap();
    assert!(shutdown.status().is_success());
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
