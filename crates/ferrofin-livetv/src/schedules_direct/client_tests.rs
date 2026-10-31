//! The Schedules Direct client against a fake SD API on the [`SourceFetcher`]
//! seam.
//!
//! The first four tests are `SchedulesDirectLineupTests` (v12.2
//! `tests/Jellyfin.LiveTv.Tests/Listings/SchedulesDirectLineupTests.cs`),
//! transliterated: upstream mocks `HttpMessageHandler` to answer `/token` and
//! `/headends` from the vendored `Test Data/SchedulesDirect` files, and so does
//! [`FakeSd`]. The rest pin the error table, the token cache, the daily limits
//! and the guide calls of `SchedulesDirect.cs`.

use std::sync::{Arc, Mutex};

use chrono::{Duration, FixedOffset, Utc};
use ferrofin_model::live_tv::{ListingsProviderInfo, ProgramAudio};
use ferrofin_traits::error::ServiceError;

use super::guide::schedule_request_dates;
use super::{API_URL, IMAGE_LIMIT_FILE, METADATA_LIMIT_FILE, SchedulesDirect};
use crate::fetch::{RequestMethod, SourceFetcher, SourceRequest, SourceResponse};

const INVALID_USER_RESPONSE: &str = r#"{"response":"INVALID_USER","code":4003,"message":"Invalid user.","serverID":"AWS-SD-web.1"}"#;

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/schedules_direct")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A fake SD API: each `(method, path)` answers its queued responses in turn,
/// repeating the last one; anything unrouted is a `404`. Every request is
/// recorded.
/// One route: `(method, path, the responses it answers in turn)`.
type Route = (RequestMethod, String, Vec<(u16, String)>);

#[derive(Default)]
struct FakeSd {
    routes: Mutex<Vec<Route>>,
    requests: Mutex<Vec<SourceRequest>>,
}

impl FakeSd {
    /// Replaces what `method path` answers.
    fn answer(&self, method: RequestMethod, path: &str, responses: &[(u16, &str)]) {
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|(m, p, _)| !(*m == method && p == path));
        routes.push((
            method,
            path.to_owned(),
            responses
                .iter()
                .map(|(status, body)| (*status, (*body).to_owned()))
                .collect(),
        ));
    }

    fn login_ok(&self) {
        self.answer(
            RequestMethod::Post,
            "/token",
            &[(200, &fixture("token_live_response.json"))],
        );
    }

    /// The requests sent to `path`.
    fn sent(&self, path: &str) -> Vec<SourceRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| route_path(&r.url) == path)
            .cloned()
            .collect()
    }
}

/// The API path of `url`, query dropped.
fn route_path(url: &str) -> &str {
    let rest = url.strip_prefix(API_URL).unwrap_or(url);
    rest.split('?').next().unwrap_or(rest)
}

#[async_trait::async_trait]
impl SourceFetcher for FakeSd {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        Err(ServiceError::backend(format!("unexpected fetch of {url}")))
    }

    async fn send(&self, request: SourceRequest) -> Result<SourceResponse, ServiceError> {
        self.requests.lock().unwrap().push(request.clone());
        let path = route_path(&request.url).to_owned();
        let mut routes = self.routes.lock().unwrap();
        let Some((_, _, responses)) = routes
            .iter_mut()
            .find(|(m, p, _)| *m == request.method && *p == path)
        else {
            return Ok(SourceResponse {
                status: 404,
                body: Vec::new(),
            });
        };
        let (status, body) = if responses.len() > 1 {
            responses.remove(0)
        } else {
            responses[0].clone()
        };
        Ok(SourceResponse {
            status,
            body: body.into_bytes(),
        })
    }
}

fn info() -> ListingsProviderInfo {
    ListingsProviderInfo {
        type_: Some("SchedulesDirect".to_owned()),
        username: Some("user".to_owned()),
        password: Some("password".into()),
        ..ListingsProviderInfo::default()
    }
}

fn provider(fake: &Arc<FakeSd>, dir: &tempfile::TempDir) -> SchedulesDirect {
    let fetcher: Arc<dyn SourceFetcher> = fake.clone();
    SchedulesDirect::new(fetcher, dir.path())
}

fn setup() -> (Arc<FakeSd>, tempfile::TempDir, SchedulesDirect) {
    let fake = Arc::new(FakeSd::default());
    let dir = tempfile::tempdir().expect("tempdir");
    let sd = provider(&fake, &dir);
    (fake, dir, sd)
}

#[tokio::test]
async fn get_lineups_valid_credentials_returns_lineups() {
    let (fake, _dir, sd) = setup();
    fake.login_ok();
    fake.answer(
        RequestMethod::Get,
        "/headends",
        &[(200, &fixture("headends_response.json"))],
    );

    let lineups = sd
        .get_lineups(&info(), Some("USA"), Some("90210"))
        .await
        .expect("lineups");

    assert!(!lineups.is_empty(), "{lineups:?}");
    assert!(
        lineups
            .iter()
            .any(|l| l.id.as_deref() == Some("USA-OTA-90210"))
    );
    assert!(lineups.iter().any(|l| l.name.as_deref() == Some("Antenna")));
    // The headend query and the token header.
    let sent = fake.sent("/headends");
    assert_eq!(
        sent[0].url,
        format!("{API_URL}/headends?country=USA&postalcode=90210")
    );
    assert_eq!(
        sent[0].headers,
        [(
            "token".to_owned(),
            "f3fca79989cafe7dead71beefedc812b".to_owned()
        )]
    );
}

#[tokio::test]
async fn get_lineups_login_fails_throws() {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[(400, INVALID_USER_RESPONSE)],
    );
    fake.answer(
        RequestMethod::Get,
        "/headends",
        &[(200, &fixture("headends_response.json"))],
    );
    // An empty lineup list is indistinguishable from "no lineups for this
    // location", so a failed login has to surface as an error instead.
    assert!(
        sd.get_lineups(&info(), Some("USA"), Some("90210"))
            .await
            .is_err()
    );
    let sent = fake.sent("/headends");
    assert!(sent.is_empty(), "{sent:?}");
}

#[tokio::test]
async fn validate_login_fails_throws() {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[(400, INVALID_USER_RESPONSE)],
    );
    assert!(sd.validate(&info(), true, false).await.is_err());
}

#[tokio::test]
async fn validate_after_account_error_recovers_without_restart() {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[(400, INVALID_USER_RESPONSE)],
    );
    fake.answer(
        RequestMethod::Get,
        "/headends",
        &[(200, &fixture("headends_response.json"))],
    );
    assert!(
        sd.get_lineups(&info(), Some("USA"), Some("90210"))
            .await
            .is_err()
    );

    // The account error disables Schedules Direct; saving the provider again is
    // the user correcting their credentials, and that has to recover without a
    // server restart.
    fake.login_ok();
    sd.validate(&info(), true, false).await.expect("validate");
    let lineups = sd
        .get_lineups(&info(), Some("USA"), Some("90210"))
        .await
        .expect("lineups");
    assert!(!lineups.is_empty(), "{lineups:?}");
}

/// An account error disables SD outright: no further login is attempted until
/// the provider is saved again, and the answer is "not authenticated" (401).
#[tokio::test]
async fn an_account_error_stops_every_later_login() {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[(400, INVALID_USER_RESPONSE)],
    );
    assert!(sd.get_lineups(&info(), None, None).await.is_err());
    let err = sd
        .get_lineups(&info(), None, None)
        .await
        .expect_err("disabled");
    assert!(matches!(err, ServiceError::Unauthorized(_)), "{err:?}");
    assert_eq!(fake.sent("/token").len(), 1);
}

/// The password goes as its lowercase SHA1 hex (jellyfin/jellyfin#17174), and
/// the token is reused instead of logging in again.
#[tokio::test]
async fn login_sends_the_lowercase_sha1_and_the_token_is_reused() {
    let (fake, _dir, sd) = setup();
    fake.login_ok();
    fake.answer(RequestMethod::Get, "/headends", &[(200, "[]")]);
    sd.get_lineups(&info(), None, None).await.expect("first");
    sd.get_lineups(&info(), None, None).await.expect("second");

    let logins = fake.sent("/token");
    assert_eq!(logins.len(), 1);
    assert_eq!(logins[0].method, RequestMethod::Post);
    assert_eq!(
        logins[0].json.as_deref(),
        Some(
            br#"{"username":"user","password":"5baa61e4c9b93f3f0682250b6cf8331b7ee68fd8"}"#
                .as_slice()
        )
    );
    // A blank country/postal code is sent empty, as `+ null` concatenates.
    assert_eq!(
        fake.sent("/headends")[0].url,
        format!("{API_URL}/headends?country=&postalcode=")
    );
}

/// `Encoding.ASCII` turns every non-ASCII UTF-16 unit into `?` before hashing.
#[test]
fn a_non_ascii_password_hashes_as_ascii_does() {
    // One `?` per character: the emoji's surrogate pair is one character.
    assert_eq!(super::ascii_bytes("pä🎉"), b"p??");
    assert_eq!(super::ascii_bytes("pass"), b"pass");
}

/// A query value is percent-encoded where a URI cannot carry it raw.
#[test]
fn postal_codes_are_query_encoded() {
    assert_eq!(super::encode_query("SW1A 1AA"), "SW1A%201AA");
    assert_eq!(super::encode_query("a&b=c"), "a%26b%3Dc");
    assert_eq!(super::encode_query("90210"), "90210");
}

/// A login answered `200` but not `"OK"` is an authentication failure (401).
#[tokio::test]
async fn a_login_that_is_not_ok_is_unauthorized() {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[(200, r#"{"code":0,"message":"Nope"}"#)],
    );
    let err = sd.validate(&info(), true, false).await.expect_err("not ok");
    assert!(matches!(err, ServiceError::Unauthorized(_)), "{err:?}");
}

/// Codes 3000/3001/4004 back off for the login window; a later save resets it.
#[tokio::test]
async fn a_transient_login_error_backs_off_until_the_provider_is_saved() {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[(400, &fixture("token_offline_response.json"))],
    );
    assert!(sd.get_lineups(&info(), None, None).await.is_err());
    // Within the back-off nothing is sent.
    assert!(sd.get_lineups(&info(), None, None).await.is_err());
    assert_eq!(fake.sent("/token").len(), 1);

    fake.login_ok();
    sd.validate(&info(), true, false)
        .await
        .expect("saved again");
    assert_eq!(fake.sent("/token").len(), 2);
}

/// A `4xx` from `/token` that no SD code explains starts the back-off too
/// (`GetToken`'s catch).
#[tokio::test]
async fn an_unexplained_4xx_login_backs_off() {
    let (fake, _dir, sd) = setup();
    fake.answer(RequestMethod::Post, "/token", &[(429, "slow down")]);
    let err = sd.get_lineups(&info(), None, None).await.expect_err("429");
    assert!(matches!(err, ServiceError::Backend(_)), "{err:?}");
    assert!(sd.get_lineups(&info(), None, None).await.is_err());
    assert_eq!(fake.sent("/token").len(), 1);
}

/// An expired token (4006), or a `403` without an SD code, is refreshed and
/// the request sent once more, with the new token.
#[rstest::rstest]
#[case(400, r#"{"code":4006}"#)]
#[case(403, "")]
#[tokio::test]
async fn an_expired_token_is_refreshed_once(#[case] status: u16, #[case] body: &str) {
    let (fake, _dir, sd) = setup();
    fake.answer(
        RequestMethod::Post,
        "/token",
        &[
            (200, r#"{"code":0,"message":"OK","token":"first"}"#),
            (200, r#"{"code":0,"message":"OK","token":"second"}"#),
        ],
    );
    let listings = ListingsProviderInfo {
        listings_id: Some("USA-OTA-90210".to_owned()),
        ..info()
    };
    fake.answer(
        RequestMethod::Get,
        "/lineups/USA-OTA-90210",
        &[(status, body), (200, &fixture("lineup_response.json"))],
    );
    let channels = sd
        .get_channels(&listings)
        .await
        .expect("channels")
        .expect("available");
    assert_eq!(channels.len(), 2);

    let sent = fake.sent("/lineups/USA-OTA-90210");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].headers[0].1, "first");
    assert_eq!(sent[1].headers[0].1, "second");

    // The retry is spent: a second failure is the error.
    fake.answer(
        RequestMethod::Get,
        "/lineups/USA-OTA-90210",
        &[(status, body)],
    );
    assert!(sd.get_channels(&listings).await.is_err());
}

/// `GetChannels`: the station map, numbered by the logical channel number,
/// else the channel, else `atscMajor.atscMinor`, leading zeros trimmed, and
/// named by the station (or the number).
#[tokio::test]
async fn channels_come_from_the_lineup_map() {
    let (fake, _dir, sd) = setup();
    fake.login_ok();
    let listings = ListingsProviderInfo {
        listings_id: Some("USA-OTA-90210".to_owned()),
        ..info()
    };
    fake.answer(
        RequestMethod::Get,
        "/lineups/USA-OTA-90210",
        &[(
            200,
            r#"{"map":[
                {"stationID":"24326","channel":"001","logicalChannelNumber":"1"},
                {"stationID":"17154","channel":"002"},
                {"stationID":"99999","atscMajor":7,"atscMinor":1}],
              "stations":[
                {"stationID":"24326","name":"BBC ONE South","callsign":"BBC1","logo":{"URL":"https://sd/logo.png"}},
                {"stationID":"17154","name":"  "}]}"#,
        )],
    );
    let channels = sd
        .get_channels(&listings)
        .await
        .expect("channels")
        .expect("available");
    let rows: Vec<_> = channels
        .iter()
        .map(|c| {
            (
                c.id.as_deref(),
                c.number.as_str(),
                c.name.as_str(),
                c.callsign.as_deref(),
                c.image_url.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                Some("24326"),
                "1",
                "BBC ONE South",
                Some("BBC1"),
                Some("https://sd/logo.png")
            ),
            (Some("17154"), "2", "2", None, None),
            (Some("99999"), "7.1", "7.1", None, None),
        ]
    );

    // No lineup chosen: no channels, and nothing is asked.
    assert_eq!(
        sd.get_channels(&info()).await.expect("none"),
        Some(Vec::new())
    );
}

/// Without a token (no credentials) the channels are "unavailable", not empty.
#[tokio::test]
async fn channels_without_credentials_are_unavailable() {
    let (fake, _dir, sd) = setup();
    let anonymous = ListingsProviderInfo {
        listings_id: Some("USA-OTA-90210".to_owned()),
        password: None,
        ..info()
    };
    assert_eq!(sd.get_channels(&anonymous).await.expect("none"), None);
    assert!(fake.requests.lock().unwrap().is_empty());
}

/// `AddLineupToAccount` is sent `ResponseHeadersRead`, and is the only
/// Schedules Direct request that is: every other one reads its whole response.
#[tokio::test]
async fn only_adding_a_lineup_ends_at_the_response_headers() {
    let (fake, _dir, sd) = setup();
    fake.login_ok();
    fake.answer(
        RequestMethod::Get,
        "/lineups",
        &[(200, &fixture("lineups_response.json"))],
    );
    fake.answer(RequestMethod::Put, "/lineups/USA-OTA-90210", &[(200, "")]);
    let with_lineup = ListingsProviderInfo {
        listings_id: Some("USA-OTA-90210".to_owned()),
        ..info()
    };
    sd.validate(&with_lineup, false, true)
        .await
        .expect("validate");

    let requests = fake.requests.lock().unwrap();
    let (puts, others): (Vec<_>, Vec<_>) = requests
        .iter()
        .partition(|request| request.method == RequestMethod::Put);
    assert_eq!(puts.len(), 1, "{requests:?}");
    assert!(puts[0].headers_only, "{requests:?}");
    assert!(!others.is_empty(), "{requests:?}");
    assert!(
        others.iter().all(|request| !request.headers_only),
        "{requests:?}"
    );
}

/// `Validate` with `validateListings`: the lineup is added to the account only
/// when it is not already there; SD's `400` for an account with no lineups is
/// "not there"; and a refusal to add it is logged, not raised.
#[tokio::test]
async fn validate_listings_adds_a_missing_lineup_to_the_account() {
    let (fake, _dir, sd) = setup();
    fake.login_ok();
    let lineups = fixture("lineups_response.json");

    for (listings_id, lineups_status, lineups_body, expect_put) in [
        ("usa-ota-60030", 200, lineups.as_str(), false),
        ("USA-OTA-90210", 200, lineups.as_str(), true),
        ("USA-OTA-90210", 400, r#"{"code":4102}"#, true),
    ] {
        fake.requests.lock().unwrap().clear();
        fake.answer(
            RequestMethod::Get,
            "/lineups",
            &[(lineups_status, lineups_body)],
        );
        fake.answer(
            RequestMethod::Put,
            &format!("/lineups/{listings_id}"),
            &[(400, r#"{"code":2100}"#)],
        );
        let with_lineup = ListingsProviderInfo {
            listings_id: Some(listings_id.to_owned()),
            ..info()
        };
        sd.validate(&with_lineup, false, true)
            .await
            .unwrap_or_else(|e| panic!("{listings_id}: {e:?}"));
        assert_eq!(
            !fake.sent(&format!("/lineups/{listings_id}")).is_empty(),
            expect_put,
            "{listings_id}"
        );
    }

    // `ThrowIfNullOrEmpty`: 400s.
    for broken in [
        ListingsProviderInfo {
            username: None,
            ..info()
        },
        ListingsProviderInfo {
            password: Some("".into()),
            ..info()
        },
    ] {
        let err = sd.validate(&broken, true, false).await.expect_err("args");
        assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
    }
    let err = sd.validate(&info(), false, true).await.expect_err("no id");
    assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
}

/// A minimal `/programs` entry.
fn details(program_id: &str, extra: &str) -> String {
    format!(
        r#"{{"programID":"{program_id}","titles":[{{"title120":"Show {program_id}"}}]{extra}}}"#
    )
}

/// The two days and the program metadata for station 20454, with airings
/// starting an hour from now.
fn guide_fixture(fake: &FakeSd) -> chrono::DateTime<Utc> {
    let start = {
        use chrono::Timelike as _;
        (Utc::now() + Duration::hours(1))
            .with_nanosecond(0)
            .expect("valid")
    };
    let at = start.format("%Y-%m-%dT%H:%M:%SZ");
    let later = (start + Duration::minutes(30)).format("%Y-%m-%dT%H:%M:%SZ");
    fake.login_ok();
    fake.answer(
        RequestMethod::Post,
        "/schedules",
        &[(
            200,
            &format!(
                r#"[{{"stationID":"20454","programs":[
                    {{"programID":"EP000014577244","airDateTime":"{at}","duration":1800,"md5":"m1",
                      "new":true,"audioProperties":["stereo","dd 5.1"],"videoProperties":["hdtv"],
                      "liveTapeDelay":"Live","isPremiereOrFinale":"Season Premiere"}},
                    {{"programID":"SH005371070000","airDateTime":"{later}","duration":3600,"md5":"m2"}}]}}]"#
            ),
        )],
    );
    let episode = details(
        "EP000014577244",
        r#","entityType":"Episode","audience":"Children","episodeTitle150":"Pilot",
           "descriptions":{"description100":[{"description":"short"}],
                           "description1000":[{"description":"long"}]},
           "originalAirDate":"2015-03-01","genres":["Comedy"," ","News"],
           "metadata":[{"Gracenote":{"season":2,"episode":3}}],
           "contentRating":[{"body":"USA Parental Rating","code":"TVPG"}],
           "hasImageArtwork":true"#,
    );
    let generic = details(
        "SH005371070000",
        r#","entityType":"Movie","movie":{"year":"1999"},"genres":["children"],
           "contentRating":[{"code":"Not Rated"}]"#,
    );
    fake.answer(
        RequestMethod::Post,
        "/programs",
        &[(200, &format!("[{episode},{generic}]"))],
    );
    fake.answer(
        RequestMethod::Post,
        "/metadata/programs/",
        &[(
            200,
            &fixture("metadata_programs_response.json").replace("SH00712240", "EP00001457"),
        )],
    );
    start
}

/// `GetProgramsAsync` + `GetProgram` over one station: the request shapes, then
/// every derived `ProgramInfo` field.
#[tokio::test]
async fn programs_are_read_and_mapped_as_get_program_does() {
    let (fake, _dir, sd) = setup();
    let start = guide_fixture(&fake);

    // A tuner-side id still decorated the SD way is normalised.
    let programs = sd
        .get_programs(
            &info(),
            "I20454.json.schedulesdirect.org",
            start - Duration::hours(1),
            start + Duration::days(1),
        )
        .await
        .expect("programs")
        .expect("available");
    assert_eq!(programs.len(), 2);

    let schedules = fake.sent("/schedules");
    let request: serde_json::Value =
        serde_json::from_slice(schedules[0].json.as_deref().expect("body")).expect("json");
    assert_eq!(request[0]["stationID"], "20454");
    assert!(request[0]["date"].as_array().is_some_and(|d| !d.is_empty()));
    assert_eq!(
        fake.sent("/programs")[0].json.as_deref(),
        Some(br#"["EP000014577244","SH005371070000"]"#.as_slice())
    );
    // Only the program with artwork is asked about, at the trailing-slash path.
    assert_eq!(
        fake.sent("/metadata/programs/")[0].json.as_deref(),
        Some(br#"["EP000014577244"]"#.as_slice())
    );

    let episode = &programs[0];
    assert_eq!(
        episode.id,
        format!(
            "EP000014577244T{}C20454",
            crate::manager::dotnet_ticks(start)
        )
    );
    assert_eq!(episode.channel_id, "20454");
    assert_eq!(
        (episode.start, episode.end),
        (start, start + Duration::minutes(30))
    );
    assert_eq!(episode.name, "Show EP000014577244");
    assert_eq!(episode.episode_title.as_deref(), Some("Pilot"));
    assert_eq!(episode.overview.as_deref(), Some("long"));
    assert_eq!(episode.genres, ["Comedy", "News"]);
    assert!(episode.is_news);
    assert_eq!(episode.official_rating.as_deref(), Some("TV-PG"));
    assert_eq!(episode.audio, ProgramAudio::DolbyDigital);
    assert!(!episode.is_repeat, "marked new");
    assert!(episode.is_series && episode.is_kids && episode.is_live && episode.is_premiere);
    assert!(episode.is_hd && !episode.is_3d && !episode.is_movie && !episode.is_sports);
    assert_eq!(episode.etag.as_deref(), Some("m1"));
    assert_eq!(episode.show_id, "EP000014577244");
    assert_eq!(episode.series_id.as_deref(), Some("EP00001457"));
    assert_eq!(
        (episode.season_number, episode.episode_number),
        (Some(2), Some(3))
    );
    assert_eq!(episode.production_year, Some(2015));
    // The 2:3 artwork is the primary; the tallest 4:3 is closest to 16:9.
    assert_eq!(
        episode.image_url.as_deref(),
        Some(
            format!(
                "{API_URL}/image/assets/p282288_b_v8_aa.jpg?token=f3fca79989cafe7dead71beefedc812b"
            )
            .as_str()
        )
    );
    assert_eq!(
        episode.thumb_image_url.as_deref(),
        Some(
            format!(
                "{API_URL}/image/assets/p282288_b_h6_aa.jpg?token=f3fca79989cafe7dead71beefedc812b"
            )
            .as_str()
        )
    );

    // A generic `SH…0000` id: a series by its id, keyed on the airing; the
    // movie year wins; an invalid rating is dropped; no `new` is a repeat.
    let generic = &programs[1];
    assert!(generic.is_series && generic.is_movie && generic.is_repeat && generic.is_kids);
    assert_eq!(generic.show_id, generic.id);
    assert_eq!(generic.series_id.as_deref(), Some("SH00537107"));
    assert_eq!(generic.official_rating, None);
    assert_eq!(generic.production_year, Some(1999));
    assert_eq!(generic.audio, ProgramAudio::Stereo);
    assert_eq!(
        (
            generic.image_url.as_deref(),
            generic.thumb_image_url.as_deref()
        ),
        (None, None)
    );
}

/// `GetProgram`'s audio ladder: element matches, not substrings.
#[rstest::rstest]
#[case(&[], ProgramAudio::Stereo)]
#[case(&["ATMOS", "dd 5.1"], ProgramAudio::Atmos)]
#[case(&["dd 5.1"], ProgramAudio::DolbyDigital)]
#[case(&["DD"], ProgramAudio::DolbyDigital)]
#[case(&["cc", "Stereo"], ProgramAudio::Stereo)]
#[case(&["cc"], ProgramAudio::Mono)]
#[case(&["stereophonic"], ProgramAudio::Mono)]
fn audio_comes_from_the_audio_properties(
    #[case] properties: &[&str],
    #[case] expected: ProgramAudio,
) {
    let airing = super::dto::ProgramDto {
        program_id: Some("EP1".to_owned()),
        air_date_time: Some(Utc::now()),
        audio_properties: properties.iter().map(|p| (*p).to_owned()).collect(),
        ..super::dto::ProgramDto::default()
    };
    let program = super::guide::program(
        "1",
        &airing,
        &super::dto::ProgramDetailsDto::default(),
        (None, None),
    )
    .expect("program");
    assert_eq!(program.audio, expected);
}

/// A metadata limit (4100) stops guide requests until SD resets; it survives a
/// restart through its marker file, and a marker from an earlier day expires.
#[tokio::test]
async fn the_metadata_limit_stops_guide_requests_and_persists() {
    let (fake, dir, sd) = setup();
    fake.login_ok();
    fake.answer(
        RequestMethod::Post,
        "/schedules",
        &[(400, r#"{"code":4100}"#)],
    );
    let window = (Utc::now(), Utc::now() + Duration::days(1));
    assert!(
        sd.get_programs(&info(), "20454", window.0, window.1)
            .await
            .is_err()
    );
    assert_eq!(
        sd.get_programs(&info(), "20454", window.0, window.1)
            .await
            .expect("limited"),
        None
    );
    assert_eq!(fake.sent("/schedules").len(), 1);
    assert!(dir.path().join(METADATA_LIMIT_FILE).exists());

    // A new process reads the marker.
    let again = provider(&fake, &dir);
    assert_eq!(
        again
            .get_programs(&info(), "20454", window.0, window.1)
            .await
            .expect("limited"),
        None
    );

    // Yesterday's marker has expired and is removed.
    let yesterday = super::round_trip(Utc::now() - Duration::days(1));
    std::fs::write(dir.path().join(METADATA_LIMIT_FILE), yesterday).expect("write");
    let fresh = provider(&fake, &dir);
    fake.answer(RequestMethod::Post, "/schedules", &[(200, "[]")]);
    fake.answer(RequestMethod::Post, "/programs", &[(200, "[]")]);
    assert_eq!(
        fresh
            .get_programs(&info(), "20454", window.0, window.1)
            .await
            .expect("programs"),
        Some(Vec::new())
    );
    assert!(!dir.path().join(METADATA_LIMIT_FILE).exists());
}

/// The login-attempt bans (4009/4010) set both daily limits.
#[tokio::test]
async fn a_login_attempt_ban_sets_both_daily_limits() {
    let (fake, dir, sd) = setup();
    fake.answer(RequestMethod::Post, "/token", &[(400, r#"{"code":4010}"#)]);
    assert!(sd.validate(&info(), true, false).await.is_err());
    assert!(sd.is_image_daily_limit_active());
    assert!(sd.is_metadata_limit_active());
    assert!(dir.path().join(IMAGE_LIMIT_FILE).exists());
    assert!(dir.path().join(METADATA_LIMIT_FILE).exists());
}

/// Artwork goes in 500-id batches. A per-entry 5003 inside a `200` response
/// sets the image limit (jellyfin/jellyfin#17347), and the next batch is not
/// sent; a failed batch is skipped rather than failing the guide.
#[tokio::test]
async fn image_batches_stop_once_the_image_limit_is_hit() {
    let (fake, dir, sd) = setup();
    fake.login_ok();
    let ids: Vec<String> = (0..501).map(|i| format!("EP{i:012}")).collect();
    fake.answer(
        RequestMethod::Post,
        "/metadata/programs/",
        &[(200, &fixture("metadata_programs_image_limit_response.json"))],
    );
    let images = sd
        .get_image_for_programs(&info(), &ids)
        .await
        .expect("images");
    // The entry with a code is dropped; the good one is kept.
    assert_eq!(images.len(), 1);
    let batches = fake.sent("/metadata/programs/");
    assert_eq!(batches.len(), 1, "the second batch is not sent");
    let first: Vec<String> =
        serde_json::from_slice(batches[0].json.as_deref().expect("body")).expect("ids");
    assert_eq!(first.len(), 500);
    assert!(sd.is_image_daily_limit_active());
    assert!(dir.path().join(IMAGE_LIMIT_FILE).exists());
    // While the limit holds, nothing is asked.
    assert!(
        sd.get_image_for_programs(&info(), &ids)
            .await
            .expect("none")
            .is_empty()
    );
    assert_eq!(fake.sent("/metadata/programs/").len(), 1);

    // A batch that fails is skipped; the next one still goes.
    let (fake, _dir, sd) = setup();
    fake.login_ok();
    fake.answer(
        RequestMethod::Post,
        "/metadata/programs/",
        &[(500, "boom"), (200, "[]")],
    );
    assert!(
        sd.get_image_for_programs(&info(), &ids)
            .await
            .expect("skipped")
            .is_empty()
    );
    assert_eq!(fake.sent("/metadata/programs/").len(), 2);
}

/// The limit marker holds `DateTime.UtcNow.ToString("O")`, which reads back.
#[test]
fn the_limit_marker_is_dotnets_round_trip_format() {
    use chrono::TimeZone as _;
    let at =
        Utc.with_ymd_and_hms(2026, 10, 21, 6, 12, 34).unwrap() + Duration::nanoseconds(123_456_700);
    let text = super::round_trip(at);
    assert_eq!(text, "2026-10-21T06:12:34.1234567Z");
    assert_eq!(super::dto::parse_datetime(&text), Some(at));
}

/// `GetScheduleRequestDates`: every day from the earlier of the window start's
/// UTC and local wall clocks to the later of its end's.
#[rstest::rstest]
#[case::utc(0, &["2026-10-21", "2026-10-22"])]
#[case::west(-6 * 3600, &["2026-10-20", "2026-10-21", "2026-10-22"])]
#[case::east(9 * 3600, &["2026-10-21", "2026-10-22", "2026-10-23"])]
fn schedule_dates_cover_both_wall_clocks(#[case] offset: i32, #[case] expected: &[&str]) {
    use chrono::TimeZone as _;
    let start = Utc.with_ymd_and_hms(2026, 10, 21, 3, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 10, 22, 20, 0, 0).unwrap();
    let zone = FixedOffset::east_opt(offset).expect("offset");
    assert_eq!(schedule_request_dates(start, end, |_| zone), expected);
}
