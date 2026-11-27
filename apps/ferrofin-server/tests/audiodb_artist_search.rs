//! TheAudioDB's half of a music artist's "Identify", end to end over real
//! HTTP (`POST /Items/RemoteSearch/MusicArtist`): v12.4
//! `AudioDbArtistProvider.GetSearchResults` looks the artist up by its
//! `AudioDbArtist` id, else its MusicBrainz id, else searches by its name,
//! and `ToRemoteSearchResult` maps every artist of the answer.
//!
//! Boots the real server ([`ferrofin_server::run`]) with TheAudioDB and
//! MusicBrainz answered by an in-process stand-in through
//! [`Config::provider_endpoints`]; every other provider stays on the offline
//! address. The stand-in's answers are trimmed recordings of TheAudioDB's
//! (`strBiographyEN` added where the live API now sends only the unsuffixed
//! `strBiography`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::{Json, Router, http::Uri};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="audiodb-search-test", Device="fixture", DeviceId="audiodb-search-test", Version="1""#;

/// Miles Davis's MusicBrainz artist id.
const MILES_MBID: &str = "561d854a-6a28-4aa7-8c99-323e6ce46c2a";

/// TheAudioDB's Miles Davis, as `artist.php`, `artist-mb.php` and
/// `search.php` answer it.
fn miles() -> Value {
    json!({
        "idArtist": "113159",
        "strArtist": "Miles Davis",
        "strArtistAlternate": "",
        "intFormedYear": "1944",
        "strGenre": "Jazz",
        "strWebsite": "www.milesdavis.com/fr",
        "strBiography": "Miles Dewey Davis III (May 26, 1926 – September 28, 1991) was an American trumpeter.",
        "strBiographyEN": "<p>Miles Dewey Davis III was an <b>American</b> trumpeter.</p>",
        "strBiographyDE": "Miles Dewey Davis III war ein US-amerikanischer Jazz-Trompeter.",
        "strArtistThumb": "https://r2.theaudiodb.com/images/media/artist/thumb/vyypuw1386451839.jpg",
        "strArtistLogo": "https://r2.theaudiodb.com/images/media/artist/logo/davis-miles-4f775bca492d7.png",
        "strMusicBrainzID": MILES_MBID,
    })
}

/// The `(path, i or s)` of each TheAudioDB request the stand-in answered.
type Asked = Arc<Mutex<Vec<(String, String)>>>;

/// A TheAudioDB and MusicBrainz stand-in.
async fn stand_in(State(asked): State<Asked>, uri: Uri) -> Json<Value> {
    let url = reqwest::Url::parse(&format!("http://stand-in{uri}")).unwrap();
    let param = |key: &str| {
        url.query_pairs()
            .find(|(name, _)| name == key)
            .map_or_default(|(_, value)| value.into_owned())
    };
    let path = url.path().rsplit('/').next().unwrap_or_default().to_owned();
    Json(match path.as_str() {
        "search.php" => {
            asked.lock().unwrap().push((path, param("s")));
            json!({"artists": [miles(), {
                "idArtist": "137285",
                "strArtist": "Miles Davis Quintet",
                "intFormedYear": "0",
                "strBiography": "An American jazz group.",
                "strArtistThumb": "",
                "strMusicBrainzID": null,
            }]})
        }
        "artist.php" | "artist-mb.php" => {
            let id = param("i");
            asked.lock().unwrap().push((path, id.clone()));
            if id == "113159" || id == MILES_MBID {
                json!({"artists": [miles()]})
            } else {
                json!({"artists": null})
            }
        }
        // MusicBrainz knows the artist by its id and finds nothing by name.
        _ if url.path() == format!("/ws/2/artist/{MILES_MBID}") => {
            json!({"id": MILES_MBID, "name": "Miles Davis", "life-span": {"begin": "1926-05-26"}})
        }
        _ => json!({"artists": []}),
    })
}

/// `POST /Items/RemoteSearch/MusicArtist` with `search_info`, the way
/// jellyfin-web's Identify dialog sends it.
async fn search(client: &reqwest::Client, base: &str, auth: &str, search_info: Value) -> Value {
    client
        .post(format!("{base}/Items/RemoteSearch/MusicArtist"))
        .header("Authorization", auth)
        .json(&json!({"SearchInfo": search_info, "IncludeDisabledProviders": true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The stand-in on a free port: its base URL, what it was asked, its task.
async fn serve_stand_in() -> (String, Asked, tokio::task::JoinHandle<()>) {
    let asked = Asked::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn({
        let router = Router::new().fallback(stand_in).with_state(asked.clone());
        async move { axum::serve(listener, router).await.unwrap() }
    });
    (endpoint, asked, mock)
}

/// The `Authorization` header of the seeded admin, signed in.
async fn sign_in(client: &reqwest::Client, base: &str) -> String {
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
    format!(
        "{CLIENT}, Token=\"{}\"",
        login["AccessToken"].as_str().unwrap()
    )
}

/// The results TheAudioDB contributed, merged ones included.
fn audiodb(results: &Value) -> Vec<&Value> {
    results
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["ProviderIds"]["AudioDbArtist"].is_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_artist_identify_lists_theaudiodb_by_its_id_its_musicbrainz_id_or_its_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (endpoint, asked, mock) = serve_stand_in().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let (server, base, _config) = boot::spawn_server(&client, |port| Config {
        port,
        musicbrainz_base_url: endpoint.clone(),
        provider_endpoints: ProviderEndpoints {
            audiodb: Some(endpoint.clone()),
            ..ProviderEndpoints::offline()
        },
        ..Config::test_stub(tmp.path())
    })
    .await;
    let auth = sign_in(&client, &base).await;

    // By name: every artist of `search.php`, mapped by `ToRemoteSearchResult`.
    let results = search(&client, &base, &auth, json!({"Name": "Miles Davis"})).await;
    assert_eq!(
        asked.lock().unwrap().drain(..).collect::<Vec<_>>(),
        [("search.php".to_owned(), "Miles Davis".to_owned())]
    );
    let found = audiodb(&results);
    assert_eq!(found.len(), 2, "{results:#}");
    assert_eq!(
        *found[0],
        json!({
            "Name": "Miles Davis",
            "ProviderIds": {"AudioDbArtist": "113159", "MusicBrainzArtist": MILES_MBID},
            "ProductionYear": 1944,
            "ImageUrl": "https://r2.theaudiodb.com/images/media/artist/thumb/vyypuw1386451839.jpg",
            "SearchProviderName": "TheAudioDB",
            // `(strBiographyEN ?? string.Empty).StripHtml()`.
            "Overview": "Miles Dewey Davis III was an American trumpeter.",
            "Artists": [],
        }),
        "{results:#}"
    );
    // No `strBiographyEN`: an empty overview, not the unsuffixed text; a
    // blank thumb is handed on as it is.
    assert_eq!(found[1]["Name"], "Miles Davis Quintet");
    assert_eq!(found[1]["Overview"], "");
    assert_eq!(found[1]["ImageUrl"], "");
    assert_eq!(found[1]["ProductionYear"], 0);
    assert_eq!(found[1]["ProviderIds"], json!({"AudioDbArtist": "137285"}));

    // A known TheAudioDB id wins over the MusicBrainz id and the name.
    let results = search(
        &client,
        &base,
        &auth,
        json!({"Name": "Miles Davis", "ProviderIds": {"AudioDbArtist": "113159", "MusicBrainzArtist": MILES_MBID}}),
    )
    .await;
    assert_eq!(
        asked.lock().unwrap().drain(..).collect::<Vec<_>>(),
        [("artist.php".to_owned(), "113159".to_owned())]
    );
    assert_eq!(audiodb(&results).len(), 1, "{results:#}");

    // Else the MusicBrainz id. MusicBrainz finds the same artist, so
    // TheAudioDB's result merges into its row by the shared id and adds its
    // own id and image.
    let results = search(
        &client,
        &base,
        &auth,
        json!({"Name": "Miles Davis", "ProviderIds": {"MusicBrainzArtist": MILES_MBID}}),
    )
    .await;
    assert_eq!(
        asked.lock().unwrap().drain(..).collect::<Vec<_>>(),
        [("artist-mb.php".to_owned(), MILES_MBID.to_owned())]
    );
    assert_eq!(results.as_array().unwrap().len(), 1, "{results:#}");
    assert_eq!(results[0]["SearchProviderName"], "MusicBrainz");
    assert_eq!(results[0]["ProviderIds"]["AudioDbArtist"], "113159");
    assert_eq!(
        results[0]["ImageUrl"],
        "https://r2.theaudiodb.com/images/media/artist/thumb/vyypuw1386451839.jpg"
    );

    // An id TheAudioDB does not know finds nothing, and the name is not
    // searched after it.
    let results = search(
        &client,
        &base,
        &auth,
        json!({"Name": "Miles Davis", "ProviderIds": {"AudioDbArtist": "999"}}),
    )
    .await;
    assert_eq!(
        asked.lock().unwrap().drain(..).collect::<Vec<_>>(),
        [("artist.php".to_owned(), "999".to_owned())]
    );
    assert!(audiodb(&results).is_empty(), "{results:#}");

    let shutdown = client
        .post(format!("{base}/System/Shutdown"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert!(shutdown.status().is_success());
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
