//! The Schedules Direct listings provider through the Live TV manager, as
//! jellyfin-web's SD page drives it (`src/components/tvproviders/schedulesdirect.js`):
//! save the account with `ValidateLogin`, pick a lineup by postal code, save it
//! with `ValidateListings`, then let the guide refresh bind SD stations to the
//! tuner's channels.
//!
//! Each test gets its own cache directory: SD persists its daily limits there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use ferrofin_db::Database;
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::stubs::LiveTvManager;

use super::{FakeDto, as_xmltv};
use crate::fetch::{RequestMethod, SourceFetcher, SourceRequest, SourceResponse};
use crate::manager::FerrofinLiveTvManager;
use crate::schedules_direct::API_URL;

/// One SD route: `(method, path, a string the JSON body must hold, status, body)`.
type Route = (RequestMethod, String, Option<String>, u16, String);

/// The tuner's playlist and the SD API: the playlist by URL, SD by
/// `(method, path)`, optionally narrowed by a string the request body holds
/// (a station id). Every SD request is recorded.
#[derive(Default)]
struct Upstream {
    sources: Mutex<HashMap<String, String>>,
    sd: Mutex<Vec<Route>>,
    sent: Mutex<Vec<SourceRequest>>,
    /// When set, each `/schedules` request waits for one permit.
    schedules_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl Upstream {
    fn serve(&self, url: &str, body: &str) {
        self.sources
            .lock()
            .unwrap()
            .insert(url.to_owned(), body.to_owned());
    }

    fn answer(&self, method: RequestMethod, path: &str, status: u16, body: &str) {
        self.answer_when(method, path, None, status, body);
    }

    /// [`answer`](Self::answer) for requests whose JSON body holds `needle`;
    /// such a route is tried before the plain one.
    fn answer_when(
        &self,
        method: RequestMethod,
        path: &str,
        needle: Option<&str>,
        status: u16,
        body: &str,
    ) {
        let mut routes = self.sd.lock().unwrap();
        routes.retain(|(m, p, n, _, _)| !(*m == method && p == path && n.as_deref() == needle));
        routes.push((
            method,
            path.to_owned(),
            needle.map(str::to_owned),
            status,
            body.to_owned(),
        ));
    }

    fn forget(&self, url: &str) {
        self.sources.lock().unwrap().remove(url);
    }

    fn sent(&self, path: &str) -> Vec<SourceRequest> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|r| sd_path(&r.url) == path)
            .cloned()
            .collect()
    }
}

fn sd_path(url: &str) -> &str {
    let rest = url.strip_prefix(API_URL).unwrap_or(url);
    rest.split('?').next().unwrap_or(rest)
}

#[async_trait::async_trait]
impl SourceFetcher for Upstream {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        self.sources
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| ServiceError::Backend(format!("no {url}")))
    }

    async fn send(&self, request: SourceRequest) -> Result<SourceResponse, ServiceError> {
        self.sent.lock().unwrap().push(request.clone());
        let path = sd_path(&request.url);
        let gate = self.schedules_gate.lock().unwrap().clone();
        if path == "/schedules"
            && let Some(gate) = gate
        {
            gate.acquire().await.expect("gate").forget();
        }
        let body = request
            .json
            .as_deref()
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        let routes = self.sd.lock().unwrap();
        let matches = |(m, p, _, _, _): &&Route| *m == request.method && p == path;
        let (status, body) = routes
            .iter()
            .filter(matches)
            .find(|(_, _, needle, _, _)| needle.as_deref().is_some_and(|n| body.contains(n)))
            .or_else(|| {
                routes
                    .iter()
                    .filter(matches)
                    .find(|(_, _, needle, _, _)| needle.is_none())
            })
            .map_or((404, String::new()), |(_, _, _, status, body)| {
                (*status, body.clone())
            });
        Ok(SourceResponse {
            status,
            body: body.into_bytes(),
        })
    }
}

async fn setup() -> (FerrofinLiveTvManager, Arc<Upstream>, tempfile::TempDir) {
    let upstream = Arc::new(Upstream::default());
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrate");
    let fetcher: Arc<dyn SourceFetcher> = upstream.clone();
    let mgr = FerrofinLiveTvManager::new(db, fetcher, "srv".to_owned(), dir.path())
        .with_dto(Arc::new(FakeDto));
    (mgr, upstream, dir)
}

/// What jellyfin-web's login step posts.
fn sd_account() -> ListingsProviderInfo {
    ListingsProviderInfo {
        type_: Some("SchedulesDirect".to_owned()),
        username: Some("user".to_owned()),
        password: Some("plain".into()),
        enable_all_tuners: true,
        path: None,
        ..ListingsProviderInfo::default()
    }
}

const TOKEN_OK: &str = r#"{"code":0,"message":"OK","token":"tok"}"#;

/// The lineup: station 20454 on channel 5, 10021 on channel 7.
const LINEUP: &str = r#"{"map":[{"stationID":"20454","logicalChannelNumber":"5"},
    {"stationID":"10021","logicalChannelNumber":"7"}],
  "stations":[{"stationID":"20454","name":"KFIVE","callsign":"KFVE"},
    {"stationID":"10021","name":"KSEVEN"}]}"#;

/// The tuner: one channel decorated with an SD station id, one SD cannot match.
const M3U: &str = "#EXTM3U\n\
    #EXTINF:-1 tvg-id=\"I20454.json.schedulesdirect.org\" tvg-chno=\"5\",Channel Five\nhttp://tuner/five\n\
    #EXTINF:-1 tvg-id=\"unmatched\" tvg-chno=\"9\",Nine\nhttp://tuner/nine\n";

/// The issue's red test: jellyfin-web's first save of an SD account has no
/// `Path`, and used to be refused with "listing provider Path is required".
/// It is stored once SD accepts the login.
#[tokio::test]
async fn a_schedules_direct_account_saves_without_a_path_after_logging_in() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);

    let saved = mgr
        .save_listing_provider(sd_account(), true, false)
        .await
        .expect("save");
    assert_eq!(saved.type_.as_deref(), Some("SchedulesDirect"));
    assert_eq!(saved.id.as_deref().map(str::len), Some(32));
    assert_eq!(upstream.sent("/token").len(), 1, "ValidateLogin logged in");

    let stored = mgr.get_listing_providers().await.expect("list");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].username.as_deref(), Some("user"));
}

/// A login SD refuses leaves nothing stored.
#[tokio::test]
async fn a_refused_login_stores_nothing() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.answer(
        RequestMethod::Post,
        "/token",
        400,
        r#"{"code":4003,"message":"Invalid user."}"#,
    );
    assert!(
        mgr.save_listing_provider(sd_account(), true, false)
            .await
            .is_err()
    );
    assert!(mgr.get_listing_providers().await.expect("list").is_empty());
}

/// `ListingsManager.GetProvider`: a missing or unknown `Type` is no provider
/// (404), and XMLTV's `Validate` refuses a local file that does not exist
/// (404) while a URL is assumed valid.
#[tokio::test]
async fn the_provider_type_and_the_xmltv_file_are_validated() {
    let (mgr, _upstream, dir) = setup().await;
    for info in [
        ListingsProviderInfo {
            type_: None,
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        },
        ListingsProviderInfo {
            type_: Some("Emby".to_owned()),
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        },
        as_xmltv(ListingsProviderInfo {
            path: Some(dir.path().join("missing.xml").display().to_string()),
            ..ListingsProviderInfo::default()
        }),
        // A directory is not a file (`File.Exists`).
        as_xmltv(ListingsProviderInfo {
            path: Some(dir.path().display().to_string()),
            ..ListingsProviderInfo::default()
        }),
    ] {
        let err = mgr
            .save_listing_provider(info.clone(), false, false)
            .await
            .expect_err("refused");
        assert!(
            matches!(err, ServiceError::NotFound(_)),
            "{info:?}: {err:?}"
        );
    }

    let guide = dir.path().join("guide.xml");
    std::fs::write(&guide, "<tv/>").expect("write");
    for path in [
        guide.display().to_string(),
        "HTTP://guide/xmltv.xml".to_owned(),
    ] {
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some(path.clone()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .unwrap_or_else(|e| panic!("{path}: {e:?}"));
    }
    assert_eq!(mgr.get_listing_providers().await.expect("list").len(), 2);
}

/// The lineup picker: a saved SD provider's lineups are SD's headends for the
/// postal code and country the dashboard sends.
#[tokio::test]
async fn lineups_are_the_headends_for_the_postal_code() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);
    upstream.answer(
        RequestMethod::Get,
        "/headends",
        200,
        r#"[{"headend":"90210","lineups":[{"name":"Antenna","lineup":"USA-OTA-90210"},
            {"name":"","uri":"/20141201/lineups/USA-CA00053-X"}]}]"#,
    );
    let id = mgr
        .save_listing_provider(sd_account(), false, false)
        .await
        .expect("save")
        .id
        .expect("id");

    let lineups = mgr
        .get_lineups(Some(&id), None, Some("USA"), Some("90210"))
        .await
        .expect("lineups");
    let pairs: Vec<_> = lineups
        .iter()
        .map(|l| (l.name.as_deref(), l.id.as_deref()))
        .collect();
    assert_eq!(
        pairs,
        [
            (Some("Antenna"), Some("USA-OTA-90210")),
            (None, Some("USA-CA00053-X")),
        ]
    );
    assert_eq!(
        upstream.sent("/headends")[0].url,
        format!("{API_URL}/headends?country=USA&postalcode=90210")
    );
}

/// The listings step: `ValidateListings` adds a lineup the account does not
/// have yet.
#[tokio::test]
async fn saving_the_lineup_adds_it_to_the_account() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);
    upstream.answer(RequestMethod::Get, "/lineups", 200, r#"{"lineups":[]}"#);
    upstream.answer(RequestMethod::Put, "/lineups/USA-OTA-90210", 200, "{}");
    mgr.save_listing_provider(
        ListingsProviderInfo {
            listings_id: Some("USA-OTA-90210".to_owned()),
            zip_code: Some("90210".to_owned()),
            country: Some("USA".to_owned()),
            ..sd_account()
        },
        false,
        true,
    )
    .await
    .expect("save");
    assert_eq!(upstream.sent("/lineups/USA-OTA-90210").len(), 1);
}

/// A tuner over [`M3U`] and an SD provider on `USA-OTA-90210`.
async fn tuner_and_sd_provider(mgr: &FerrofinLiveTvManager, upstream: &Upstream) -> String {
    upstream.serve("http://tuner/playlist.m3u", M3U);
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);
    upstream.answer(RequestMethod::Get, "/lineups/USA-OTA-90210", 200, LINEUP);
    mgr.save_tuner_host(TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some("http://tuner/playlist.m3u".to_owned()),
        ..TunerHostInfo::default()
    })
    .await
    .expect("tuner");
    mgr.save_listing_provider(
        ListingsProviderInfo {
            listings_id: Some("USA-OTA-90210".to_owned()),
            ..sd_account()
        },
        false,
        false,
    )
    .await
    .expect("provider")
    .id
    .expect("id")
}

/// The channel-mapping dialog names SD and lists the lineup's stations.
#[tokio::test]
async fn channel_mapping_lists_the_lineups_stations() {
    let (mgr, upstream, _dir) = setup().await;
    let id = tuner_and_sd_provider(&mgr, &upstream).await;
    mgr.refresh_guide().await.expect("refresh");

    let options = mgr.get_channel_mapping_options(&id).await.expect("options");
    assert_eq!(options.provider_name.as_deref(), Some("Schedules Direct"));
    let stations: Vec<_> = options
        .provider_channels
        .iter()
        .map(|c| (c.id.as_deref(), c.name.as_deref()))
        .collect();
    assert_eq!(
        stations,
        [
            (Some("20454"), Some("KFIVE")),
            (Some("10021"), Some("KSEVEN"))
        ]
    );
    // The decorated tvg-id binds Channel Five to station 20454.
    let five = options
        .tuner_channels
        .iter()
        .find(|c| c.name.as_deref() == Some("5 Channel Five"))
        .expect("channel five");
    assert_eq!(five.provider_channel_id.as_deref(), Some("20454"));
}

fn airing_at(offset_minutes: i64) -> String {
    use chrono::Timelike as _;
    (Utc::now() + Duration::minutes(offset_minutes))
        .with_nanosecond(0)
        .expect("valid")
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// The guide refresh asks SD only for the station a tuner channel binds to,
/// and stores its airings the way `GuideManager.GetProgram` does.
#[tokio::test]
async fn the_guide_refresh_stores_the_bound_stations_airings() {
    let (mgr, upstream, _dir) = setup().await;
    tuner_and_sd_provider(&mgr, &upstream).await;
    upstream.answer(
        RequestMethod::Post,
        "/schedules",
        200,
        &format!(
            r#"[{{"stationID":"20454","programs":[
                {{"programID":"EP000014577244","airDateTime":"{}","duration":1800,"new":true}},
                {{"programID":"MV000000010000","airDateTime":"{}","duration":7200}}]}}]"#,
            airing_at(60),
            airing_at(90)
        ),
    );
    upstream.answer(
        RequestMethod::Post,
        "/programs",
        200,
        r#"[{"programID":"EP000014577244","titles":[{"title120":"Show"}],"entityType":"Episode",
             "episodeTitle150":"Pilot","genres":["News"],"metadata":[{"Gracenote":{"season":1,"episode":2}}]},
            {"programID":"MV000000010000","titles":[{"title120":"Film"}],"entityType":"Movie",
             "movie":{"year":"1999"}}]"#,
    );

    mgr.refresh_guide().await.expect("refresh");

    // Only station 20454 was asked about: 10021 has no tuner channel.
    let schedules = upstream.sent("/schedules");
    assert_eq!(schedules.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(schedules[0].json.as_deref().expect("body")).expect("json");
    assert_eq!(body[0]["stationID"], "20454");

    let programs = mgr
        .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
        .await
        .expect("programs");
    assert_eq!(programs.total_record_count, 2);
    let by_name = |name: &str| {
        programs
            .items
            .iter()
            .find(|p| p.name.as_deref() == Some(name))
            .cloned()
            .expect(name)
    };
    let episode = by_name("Show");
    assert_eq!(episode.is_series, Some(true));
    assert_eq!(episode.is_news, Some(true));
    assert_eq!(episode.episode_title.as_deref(), Some("Pilot"));
    assert_eq!(
        (episode.parent_index_number, episode.index_number),
        (Some(1), Some(2))
    );
    assert_eq!(episode.is_repeat, None, "marked new");
    assert_eq!(episode.run_time_ticks, Some(18_000_000_000));
    let movie = by_name("Film");
    assert_eq!(movie.is_movie, Some(true));
    assert_eq!(movie.is_repeat, Some(true));
    assert_eq!(movie.production_year, Some(1999));
    // Both on the one bound channel.
    assert!(episode.channel_id.is_some());
    assert_eq!(episode.channel_id, movie.channel_id);
}

/// How many airings the guide holds.
async fn count(mgr: &FerrofinLiveTvManager) -> usize {
    mgr.get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
        .await
        .expect("programs")
        .items
        .len()
}

/// While SD is unavailable (here its metadata daily limit), a refresh keeps
/// the stored guide instead of draining it, and asks SD nothing more.
#[tokio::test]
async fn an_unavailable_schedules_direct_keeps_the_stored_guide() {
    let (mgr, upstream, _dir) = setup().await;
    tuner_and_sd_provider(&mgr, &upstream).await;
    upstream.answer(
        RequestMethod::Post,
        "/schedules",
        200,
        &format!(
            r#"[{{"stationID":"20454","programs":[{{"programID":"EP000014577244","airDateTime":"{}","duration":1800}}]}}]"#,
            airing_at(60)
        ),
    );
    upstream.answer(
        RequestMethod::Post,
        "/programs",
        200,
        r#"[{"programID":"EP000014577244","titles":[{"title120":"Show"}]}]"#,
    );
    mgr.refresh_guide().await.expect("first refresh");
    assert_eq!(count(&mgr).await, 1);

    upstream.answer(RequestMethod::Post, "/schedules", 400, r#"{"code":4100}"#);
    mgr.refresh_guide().await.expect("limited refresh");
    assert_eq!(count(&mgr).await, 1, "the stored guide is kept");
    mgr.refresh_guide().await.expect("refresh within the limit");
    assert_eq!(count(&mgr).await, 1);
    assert_eq!(
        upstream.sent("/schedules").len(),
        2,
        "no request while limited"
    );
}

/// One airing an hour out, as a `/schedules` response.
fn one_airing() -> String {
    format!(
        r#"[{{"stationID":"20454","programs":[{{"programID":"EP000014577244","airDateTime":"{}","duration":1800}}]}}]"#,
        airing_at(60)
    )
}

/// The `/programs` details of that airing, titled `title`.
fn titled(title: &str) -> String {
    format!(r#"[{{"programID":"EP000014577244","titles":[{{"title120":"{title}"}}]}}]"#)
}

/// The airings' `(channel id, name)`, sorted.
async fn airings(mgr: &FerrofinLiveTvManager) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = mgr
        .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
        .await
        .expect("programs")
        .items
        .into_iter()
        .map(|p| {
            (
                p.channel_id.map(|id| id.to_string()).unwrap_or_default(),
                p.name.unwrap_or_default(),
            )
        })
        .collect();
    out.sort();
    out
}

/// A station that two tuner channels bind to is asked for once, and its
/// airings go onto both channels (`ListingsManager` builds one programme per
/// tuner channel, suffixing the id with the channel's).
#[tokio::test]
async fn one_station_feeds_every_channel_bound_to_it() {
    let (mgr, upstream, _dir) = setup().await;
    tuner_and_sd_provider(&mgr, &upstream).await;
    upstream.serve(
        "http://tuner/playlist.m3u",
        "#EXTM3U\n\
         #EXTINF:-1 tvg-id=\"I20454.json.schedulesdirect.org\",Five East\nhttp://tuner/five-east\n\
         #EXTINF:-1 tvg-id=\"I20454.json.schedulesdirect.org\",Five West\nhttp://tuner/five-west\n",
    );
    upstream.answer(RequestMethod::Post, "/schedules", 200, &one_airing());
    upstream.answer(RequestMethod::Post, "/programs", 200, &titled("Show"));
    mgr.refresh_guide().await.expect("refresh");

    assert_eq!(upstream.sent("/schedules").len(), 1);
    let rows = airings(&mgr).await;
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].0, rows[1].0, "one airing per channel");
    assert!(rows.iter().all(|(_, name)| name == "Show"));
}

/// A provider enabled for one tuner binds only that tuner's channels
/// (`IsListingProviderEnabledForTuner`), so SD is asked only for their
/// stations.
#[tokio::test]
async fn a_provider_enabled_for_one_tuner_binds_only_its_channels() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);
    upstream.answer(RequestMethod::Get, "/lineups/USA-OTA-90210", 200, LINEUP);
    upstream.serve(
        "http://tuner/a.m3u",
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"I20454.json.schedulesdirect.org\",Five\nhttp://tuner/five\n",
    );
    upstream.serve(
        "http://tuner/b.m3u",
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"I10021.json.schedulesdirect.org\",Seven\nhttp://tuner/seven\n",
    );
    let mut ids = Vec::new();
    for url in ["http://tuner/a.m3u", "http://tuner/b.m3u"] {
        ids.push(
            mgr.save_tuner_host(TunerHostInfo {
                type_: Some("m3u".to_owned()),
                url: Some(url.to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("tuner")
            .id
            .expect("id"),
        );
    }
    mgr.save_listing_provider(
        ListingsProviderInfo {
            listings_id: Some("USA-OTA-90210".to_owned()),
            enable_all_tuners: false,
            enabled_tuners: vec![ids[0].clone()],
            ..sd_account()
        },
        false,
        false,
    )
    .await
    .expect("provider");
    upstream.answer(RequestMethod::Post, "/schedules", 200, &one_airing());
    upstream.answer(RequestMethod::Post, "/programs", 200, &titled("Show"));
    mgr.refresh_guide().await.expect("refresh");

    let schedules = upstream.sent("/schedules");
    assert_eq!(schedules.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(schedules[0].json.as_deref().expect("body")).expect("json");
    assert_eq!(body[0]["stationID"], "20454");
    assert_eq!(airings(&mgr).await.len(), 1);
}

/// A tuner whose playlist cannot be read binds no station this pass, so SD
/// is not asked, and the refresh keeps the guide its channels had.
#[tokio::test]
async fn a_tuner_that_cannot_be_read_keeps_its_channels_guide() {
    let (mgr, upstream, _dir) = setup().await;
    tuner_and_sd_provider(&mgr, &upstream).await;
    upstream.answer(RequestMethod::Post, "/schedules", 200, &one_airing());
    upstream.answer(RequestMethod::Post, "/programs", 200, &titled("Show"));
    mgr.refresh_guide().await.expect("first refresh");
    assert_eq!(airings(&mgr).await.len(), 1);

    upstream.forget("http://tuner/playlist.m3u");
    mgr.refresh_guide()
        .await
        .expect("refresh without the tuner");
    assert_eq!(airings(&mgr).await.len(), 1, "the stored guide is kept");
    assert_eq!(upstream.sent("/schedules").len(), 1);
}

/// A station SD fails to answer keeps its stored airings while the stations
/// read this pass are replaced (`RefreshChannelsInternal` goes on per
/// channel; `hasErrors` keeps the clean-up from draining the rest).
#[tokio::test]
async fn a_failing_station_keeps_its_guide_while_the_others_refresh() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.serve(
        "http://tuner/playlist.m3u",
        "#EXTM3U\n\
         #EXTINF:-1 tvg-id=\"I20454.json.schedulesdirect.org\",Five\nhttp://tuner/five\n\
         #EXTINF:-1 tvg-id=\"I10021.json.schedulesdirect.org\",Seven\nhttp://tuner/seven\n",
    );
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);
    upstream.answer(RequestMethod::Get, "/lineups/USA-OTA-90210", 200, LINEUP);
    mgr.save_tuner_host(TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some("http://tuner/playlist.m3u".to_owned()),
        ..TunerHostInfo::default()
    })
    .await
    .expect("tuner");
    mgr.save_listing_provider(
        ListingsProviderInfo {
            listings_id: Some("USA-OTA-90210".to_owned()),
            ..sd_account()
        },
        false,
        false,
    )
    .await
    .expect("provider");
    upstream.answer(RequestMethod::Post, "/schedules", 200, &one_airing());
    upstream.answer(RequestMethod::Post, "/programs", 200, &titled("Old"));
    mgr.refresh_guide().await.expect("first refresh");
    assert_eq!(airings(&mgr).await.len(), 2);

    upstream.answer_when(RequestMethod::Post, "/schedules", Some("10021"), 500, "");
    upstream.answer(RequestMethod::Post, "/programs", 200, &titled("New"));
    mgr.refresh_guide().await.expect("partial refresh");
    let names: Vec<String> = airings(&mgr).await.into_iter().map(|(_, n)| n).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, ["New", "Old"], "{names:?}");
}

/// Waits (bounded) until `upstream` has seen `count` `/schedules` requests.
async fn until_schedules_sent(upstream: &Upstream, count: usize) {
    for _ in 0..500 {
        if upstream.sent("/schedules").len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{count} /schedules requests never came");
}

/// `CancelIfRunningAndQueue`: a refresh started while another is still asking
/// SD for stations supersedes it. The older pass stops before its next station
/// and writes nothing; the newer one asks for every station and writes the
/// guide. Two stations, two passes: three `/schedules` requests, not four.
#[tokio::test]
async fn a_newer_refresh_supersedes_one_still_asking_schedules_direct() {
    let (mgr, upstream, _dir) = setup().await;
    upstream.serve(
        "http://tuner/playlist.m3u",
        "#EXTM3U\n\
         #EXTINF:-1 tvg-id=\"I20454.json.schedulesdirect.org\",Five\nhttp://tuner/five\n\
         #EXTINF:-1 tvg-id=\"I10021.json.schedulesdirect.org\",Seven\nhttp://tuner/seven\n",
    );
    upstream.answer(RequestMethod::Post, "/token", 200, TOKEN_OK);
    upstream.answer(RequestMethod::Get, "/lineups/USA-OTA-90210", 200, LINEUP);
    upstream.answer(RequestMethod::Post, "/schedules", 200, &one_airing());
    upstream.answer(RequestMethod::Post, "/programs", 200, &titled("Show"));
    mgr.save_tuner_host(TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some("http://tuner/playlist.m3u".to_owned()),
        ..TunerHostInfo::default()
    })
    .await
    .expect("tuner");
    mgr.save_listing_provider(
        ListingsProviderInfo {
            listings_id: Some("USA-OTA-90210".to_owned()),
            ..sd_account()
        },
        false,
        false,
    )
    .await
    .expect("provider");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *upstream.schedules_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let mgr = Arc::new(mgr);
    let first = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { mgr.refresh_guide().await }
    });
    until_schedules_sent(&upstream, 1).await;
    let second = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        async move { mgr.refresh_guide().await }
    });
    until_schedules_sent(&upstream, 2).await;
    gate.add_permits(100);
    first.await.expect("join").expect("first refresh");
    second.await.expect("join").expect("second refresh");

    assert_eq!(upstream.sent("/schedules").len(), 3);
    assert_eq!(airings(&mgr).await.len(), 2);
}
