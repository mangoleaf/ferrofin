//! The statements a request runs, end to end.
//!
//! Each test boots the real composition root over a temp data dir with a
//! scanned movies library (and the playlists folder every server provisions),
//! and records, through a global subscriber, the `sqlx::query` events emitted
//! inside one request's span (the SQLite worker enters the caller's span).
//!
//! - `GET /Users/{id}/Views` (gl-981). The home screen's view row asks three
//!   questions about the libraries' physical folders: the scope of 12.2's
//!   `HasVisibleItem` probe for the playlists folder, that probe's own query,
//!   and the views' `ChildCount` (`GetChildCountSourceIds`). All three read
//!   `CollectionFolder` rows, which the library set the visibility check caches
//!   already holds, `Data` included. The probe's scope also looks for the Live
//!   TV view among the `UserView` rows it has just read. So a warm request reads
//!   neither again. Then, the set still warm, it toggles the library's
//!   `Enabled` option through the dashboard's endpoint and checks the user's
//!   scope follows on the next request.
//! - A library browse whose page holds a movie with versions (gl-1083). Its
//!   version resume (`GetResumeUserDataBatch`) takes the page's own items' user
//!   data from the page, as upstream's cache answers them, and reads only what
//!   the user stored on the versions off the page: none here, so no user-data
//!   key is derived.
//! - `GET /Users/{id}/Views` on a server holding the Live TV view (gl-1154).
//!   The Live TV gate (`IsLiveTvEnabled`: the user's permission, and whether a
//!   tuner is configured) is asked three times by one views request, and
//!   upstream answers it from the `LiveTvOptions` it holds in memory. So the
//!   tuner hosts are read once after a write to them, and not again while
//!   that answer is cached; a tuner added or removed through the API reaches
//!   the very next request.
//! - `GET /Items?parentId=` on a library and on a box set (gl-1153). The
//!   handler's parent visibility check reads the parent's row, and the query
//!   carries it, so the repository's scope resolution, the folder listing
//!   and the box set's display order work from it: the parent's row is read
//!   once, as upstream's controller reads it once (`GetParentItem`).
//! - `POST /Sessions/Playing`, then `GET /Sessions` twice (gl-1242). The start
//!   report builds no `NowPlayingItem`: the first listing builds it, from the
//!   row the report read, and the second copies it.
//! - `GET /Sessions`, plain and with `controllableByUserId`, from an
//!   administrator and from a user (gl-1265). The permissions the listing
//!   checks come from the policy cache, as upstream reads them from the user
//!   it holds, and the caller's row is read once; a policy saved through the
//!   API reaches the very next listing.
//! - The user's views with the Live TV view and the user's playlists view
//!   (gl-1322). Both rows are in the listing of view rows the read takes, so a
//!   warm read looks neither up by id; a row deleted since is made again.
//! - `GET /Items?Fields=ProviderIds`, `GET /Items/{id}` and
//!   `GET /Items?Fields=ExternalUrls` as a user (gl-1323). The DTO's provider
//!   ids and the key of the user data the user never wrote come from the
//!   page's `BaseItemProviders` rows, read once, as upstream's both come from
//!   the item's `Provider` navigation.
//! - A series' episodes and seasons with `ExternalUrls` (gl-1390): with and
//!   without user data, with `ProviderIds`, an episode's own page, and a page
//!   listing the series too, in both orders. The links come from the series'
//!   provider ids as well as the items' own, and both are read in one
//!   `BaseItemProviders` query.
//! - An unscoped `GET /Items?Recursive=true` (gl-1283). Its scope, the user's
//!   libraries, views and playlists folder, comes from the cached library set,
//!   as upstream holds them in memory: a warm listing reads none of those rows.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use tower::ServiceExt as _;
use tracing::Instrument as _;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "request-statements-pw";

/// The span the recorded views request runs in.
const VIEWS_SPAN: &str = "gl981_views_request";

/// The span the recorded browse with a version group runs in.
const VERSIONS_SPAN: &str = "gl1083_version_resume_request";

/// The span the recorded views requests around a tuner's writes run in.
const LIVE_TV_SPAN: &str = "gl1154_live_tv_views_request";

/// The span the recorded browses of a parent run in.
const PARENT_SPAN: &str = "gl1153_parent_browse_request";

/// The span the recorded playback start reports run in.
const PLAYING_SPAN: &str = "gl1242_playing_request";

/// The span the recorded session listings run in.
const SESSIONS_SPAN: &str = "gl1265_sessions_request";

/// The span the recorded views reads holding the Live TV and playlists views
/// run in.
const VIEW_ROWS_SPAN: &str = "gl1322_view_rows_read";

/// The span the recorded item reads with provider ids run in.
const PROVIDERS_SPAN: &str = "gl1323_provider_ids_request";

/// The span the recorded episode listings with links run in.
const SERIES_PROVIDERS_SPAN: &str = "gl1390_series_provider_ids_request";

/// The span the recorded unscoped listing runs in.
const UNSCOPED_SPAN: &str = "gl1283_unscoped_listing_request";

/// The statements recorded inside each test's span, in order, tagged with the
/// span: `cargo test` runs a binary's tests on threads of one process.
static STATEMENTS: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

/// Records the text of every `sqlx::query` event emitted inside one of the
/// recorded spans.
struct Statements;

impl<S> tracing_subscriber::Layer<S> for Statements
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let Some(span) = ctx.event_scope(event).and_then(|mut scope| {
            scope.find_map(|span| {
                [
                    VIEWS_SPAN,
                    VERSIONS_SPAN,
                    LIVE_TV_SPAN,
                    PARENT_SPAN,
                    PLAYING_SPAN,
                    SESSIONS_SPAN,
                    VIEW_ROWS_SPAN,
                    PROVIDERS_SPAN,
                    SERIES_PROVIDERS_SPAN,
                    UNSCOPED_SPAN,
                ]
                .into_iter()
                .find(|name| span.name() == *name)
            })
        }) else {
            return;
        };
        let mut text = StatementText(String::new());
        event.record(&mut text);
        STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                span,
                text.0.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
    }
}

/// Forgets what was recorded inside `span`.
fn clear_statements(span: &str) {
    STATEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(recorded, _)| *recorded != span);
}

/// The statements recorded inside `span`, in order.
fn statements_in(span: &str) -> Vec<String> {
    STATEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(recorded, _)| *recorded == span)
        .map(|(_, statement)| statement.clone())
        .collect()
}

/// sqlx's `summary` (the statement's start) and `db.statement` (the whole
/// statement, when the summary is cut short).
struct StatementText(String);

impl tracing::field::Visit for StatementText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if matches!(field.name(), "summary" | "db.statement") {
            self.0.push_str(value);
            self.0.push(' ');
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "summary" | "db.statement") {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{value:?} ");
        }
    }
}

/// A booted server plus what the test drives it with.
struct Harness {
    wired: WiredApp,
    /// The server's database, whose cached library set the test checks.
    db: Database,
    /// The movies library's media directory.
    movies_dir: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the admin, with a
/// movies directory holding `files` (paths under it).
async fn boot(files: &[&str]) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-request-statements".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let movies_dir = temp.path().join("movies");
    for file in files {
        let path = movies_dir.join(file);
        std::fs::create_dir_all(path.parent().expect("movie dir")).expect("movie dir");
        std::fs::write(path, [0u8; 1024]).expect("movie file");
    }
    std::fs::create_dir_all(&config.data_dir).expect("data dir");
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    Harness {
        wired,
        db,
        movies_dir: movies_dir.to_string_lossy().into_owned(),
        _temp: temp,
    }
}

/// Sends `method uri` with `auth_header` and an optional JSON body; returns
/// status + JSON body.
async fn send(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth_header);
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("valid JSON body")
    };
    (status, json)
}

/// Percent-encodes a path for a query string, keeping `/`.
fn urlencoding(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Installs [`Statements`] as the process's subscriber, once.
fn record_statements() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Statements))
            .expect("no other subscriber in this binary");
    });
}

/// Signs `user` in from `device_id`; returns the authorization header and the
/// user's id.
async fn sign_in(
    router: &axum::Router,
    user: &str,
    password: &str,
    device_id: &str,
) -> (String, String) {
    let client = format!(
        r#"MediaBrowser Client="Request Statements", Device="Test Rig", DeviceId="{device_id}", Version="1.0.0""#
    );
    let (status, auth) = send(
        router,
        &client,
        "POST",
        "/Users/AuthenticateByName",
        Some(serde_json::json!({ "Username": user, "Pw": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{user} authenticates: {auth}");
    let token = auth["AccessToken"].as_str().expect("token");
    let user_id = auth["User"]["Id"].as_str().expect("user id").to_owned();
    (format!(r#"{client}, Token="{token}""#), user_id)
}

/// Signs the admin in, adds the movies library with no remote fetchers and
/// scans it; returns the admin's authorization header and user id.
async fn sign_in_and_scan(harness: &Harness, router: &axum::Router) -> (String, String) {
    let (auth_header, user_id) = sign_in(router, ADMIN_USER, ADMIN_PASSWORD, "rig-rs").await;

    // No remote metadata or image fetchers: a scan that asks TMDb waits on it.
    let (status, body) = send(
        router,
        &auth_header,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Movies&collectionType=movies&paths={}&refreshLibrary=false",
            urlencoding(&harness.movies_dir)
        ),
        Some(serde_json::json!({"LibraryOptions": {"TypeOptions": [
            {"Type": "Movie", "MetadataFetchers": [], "ImageFetchers": []}
        ]}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    // Awaited, not queued: the end of a scan writes the library row, which
    // drops the cached library set the recorded request is meant to find.
    assert!(
        harness
            .wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan"),
        "the scan ran"
    );
    (auth_header, user_id)
}

/// Adds a shows library holding one episode of a series named with its Tmdb
/// and Tvdb ids, with no remote fetchers, and scans it; returns the series' id.
async fn scan_a_show(
    harness: &Harness,
    router: &axum::Router,
    auth_header: &str,
    user_id: &str,
) -> String {
    let shows = std::path::Path::new(&harness.movies_dir).with_file_name("shows");
    let episode = shows.join("Show (2008) [tmdbid-1396] [tvdbid-81189]/Season 01/Show S01E01.mkv");
    std::fs::create_dir_all(episode.parent().expect("season dir")).expect("season dir");
    std::fs::write(&episode, [0u8; 1024]).expect("episode file");
    let (status, body) = send(
        router,
        auth_header,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Shows&collectionType=tvshows&paths={}&refreshLibrary=false",
            urlencoding(&shows.to_string_lossy())
        ),
        Some(serde_json::json!({"LibraryOptions": {"TypeOptions": [
            {"Type": "Series", "MetadataFetchers": [], "ImageFetchers": []},
            {"Type": "Season", "MetadataFetchers": [], "ImageFetchers": []},
            {"Type": "Episode", "MetadataFetchers": [], "ImageFetchers": []}
        ]}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    assert!(
        harness
            .wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan"),
        "the scan ran"
    );
    let (status, series) = send(
        router,
        auth_header,
        "GET",
        &format!("/Items?userId={user_id}&includeItemTypes=Series&recursive=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "series: {series}");
    series["Items"][0]["Id"]
        .as_str()
        .expect("the series")
        .to_owned()
}

#[tokio::test]
async fn a_warm_views_request_reads_no_library_row_again() {
    record_statements();
    let harness = boot(&[
        "Film Alpha (1999)/Film Alpha (1999).mkv",
        "Film Beta (2004)/Film Beta (2004).mkv",
    ])
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let views_uri = format!("/Users/{user_id}/Views");
    let movies_count = |views: &serde_json::Value| {
        views["Items"].as_array().and_then(|items| {
            items
                .iter()
                .find(|item| item["Name"] == "Movies")
                .and_then(|item| item["ChildCount"].as_i64())
        })
    };

    // Warm: the visibility check has cached the library set.
    let (status, warm) = send(&router, &auth_header, "GET", &views_uri, None).await;
    assert_eq!(status, StatusCode::OK, "views: {warm}");
    assert!(
        harness.db.cached_libraries().is_some(),
        "a views request leaves the library set cached"
    );
    clear_statements(VIEWS_SPAN);
    let (status, views) = send(&router, &auth_header, "GET", &views_uri, None)
        .instrument(tracing::info_span!(VIEWS_SPAN))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(views, warm, "the recorded request answers as the warm one");
    assert_eq!(movies_count(&views), Some(2), "{views}");
    let statements = statements_in(VIEWS_SPAN);
    let listing = statements.join("\n");
    // The recording sees the request: its child count is there.
    assert!(
        statements
            .iter()
            .any(|s| s.contains(r#"SELECT bi."ParentId", COUNT(*)"#)),
        "the views' child count is recorded:\n{listing}"
    );
    assert!(
        !statements
            .iter()
            .any(|s| s.contains(r#"WHERE "Data" IS NOT NULL AND +"Type" ="#)),
        "the libraries' physical folders come from the cached library set:\n{listing}"
    );
    assert!(
        !statements
            .iter()
            .any(|s| s.contains(r#"WHERE "Type" = ?1 AND ("Path" LIKE"#)),
        "the Live TV view is found among the rows the scope read:\n{listing}"
    );
    assert!(
        harness.db.cached_libraries().is_some(),
        "the set stayed cached through the recorded request"
    );

    a_dashboard_toggle_reaches_the_next_request(&harness, &router, &auth_header, &user_id).await;
}

/// An unscoped `GET /Items?Recursive=true&IncludeItemTypes=Movie` (gl-1283):
/// `AddUserToQuery` scopes it to the user's views, the libraries, the
/// playlists folder and the `UserView` rows, which upstream holds in memory.
/// The library set the visibility check caches holds all of them, so a warm
/// listing reads none of their rows again.
#[tokio::test]
async fn a_warm_unscoped_listing_reads_no_library_or_view_row_again() {
    record_statements();
    let harness = boot(&[
        "Film Alpha (1999)/Film Alpha (1999).mkv",
        "Film Beta (2004)/Film Beta (2004).mkv",
    ])
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let uri = format!("/Items?Recursive=true&IncludeItemTypes=Movie&userId={user_id}");

    let (status, warm) = send(&router, &auth_header, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "listing: {warm}");
    assert_eq!(warm["TotalRecordCount"], 2, "{warm}");
    assert!(
        harness.db.cached_libraries().is_some(),
        "a listing leaves the library set cached"
    );
    clear_statements(UNSCOPED_SPAN);
    let (status, page) = send(&router, &auth_header, "GET", &uri, None)
        .instrument(tracing::info_span!(UNSCOPED_SPAN))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page, warm, "the recorded request answers as the warm one");
    let statements = statements_in(UNSCOPED_SPAN);
    let listing = statements.join("\n");
    assert!(
        statements
            .iter()
            .any(|s| s.contains(r#"bi."TopParentId" IN ("#)),
        "the scoped listing is recorded:\n{listing}"
    );
    assert!(
        !statements
            .iter()
            .any(|s| s.contains(r#"FROM "BaseItems" WHERE "Type" IN ("#)),
        "the scope's libraries, views and playlists folder come from the cached \
         library set:\n{listing}"
    );
}

/// A movie with two versions on a library browse, the runtime-sorted edge
/// request of the release gate: the page's version resume
/// (`GetResumeUserDataBatch`, `DtoService.cs:171-180`) takes the primary's
/// user data from the page, as upstream's `GetUserDataBatch` answers it from
/// the cache the page's batch filled, and reads only what the user stored on
/// the version off the page. Nothing is stored, so no user-data key is
/// derived: the key read (`identity_ids`) exists to choose among stored rows
/// and to key a never-written row, which nothing here shows.
#[tokio::test]
async fn a_version_group_s_resume_derives_no_key_for_unwritten_versions() {
    record_statements();
    let harness = boot(&[
        "Film Alpha (1999)/Film Alpha (1999).mkv",
        "Film Gamma (2010)/Film Gamma (2010) - 1080p.mkv",
        "Film Gamma (2010)/Film Gamma (2010) - 2160p.mkv",
    ])
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let (status, views) = send(
        &router,
        &auth_header,
        "GET",
        &format!("/Users/{user_id}/Views"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "views: {views}");
    let movies = views["Items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["Name"] == "Movies"))
        .and_then(|item| item["Id"].as_str())
        .expect("the Movies view")
        .to_owned();
    let browse = format!(
        "/Items?userId={user_id}&parentId={movies}&includeItemTypes=Movie&recursive=true\
         &sortBy=Runtime,SortName&sortOrder=Ascending&limit=50"
    );
    let (status, warm) = send(&router, &auth_header, "GET", &browse, None).await;
    assert_eq!(status, StatusCode::OK, "browse: {warm}");
    let gammas = warm["Items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["Name"] == "Film Gamma (2010)")
        .count();
    assert_eq!(gammas, 1, "the versions list once: {warm}");

    clear_statements(VERSIONS_SPAN);
    let (status, page) = send(&router, &auth_header, "GET", &browse, None)
        .instrument(tracing::info_span!(VERSIONS_SPAN))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page, warm, "the recorded request answers as the warm one");
    let statements = statements_in(VERSIONS_SPAN);
    let listing = statements.join("\n");
    // The page's version group is read, so the resume had a group to fold,
    // and then the user data of the one version off the page, alone: the
    // grouped movie on the page answers with the page's own.
    let versions_read = statements
        .iter()
        .position(|s| s.contains(r#"AS "HasVersions""#))
        .unwrap_or_else(|| panic!("the page's versions are read:\n{listing}"));
    assert!(
        statements[versions_read..]
            .iter()
            .any(|s| s.contains(r#"FROM "UserData""#) && s.contains(r#""ItemId" IN (?2)"#)),
        "the version off the page is read, alone:\n{listing}"
    );
    assert!(
        !statements.iter().any(|s| s.contains("identity_ids")),
        "no user-data key is derived for a version nobody played:\n{listing}"
    );
}

/// The Live TV gate behind a views request, on a server whose database holds
/// the Live TV view: upstream's `IsLiveTvEnabled` asks its in-memory
/// `LiveTvOptions.TunerHosts`, so the tuner hosts are read once after a write
/// to them and not again while that answer is cached, and a tuner added or
/// removed through the dashboard's endpoints reaches the very next request.
#[tokio::test]
async fn the_live_tv_gate_reads_the_tuner_hosts_once_and_follows_their_writes() {
    record_statements();
    let harness = boot(&["Film Alpha (1999)/Film Alpha (1999).mkv"]).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let views_uri = format!("/Users/{user_id}/Views");

    let (listed, _) = live_tv_views(&router, &auth_header, &views_uri).await;
    assert!(!listed, "no tuner, no Live TV view");

    let playlist = std::path::Path::new(&harness.movies_dir).with_file_name("tuner.m3u");
    std::fs::write(
        &playlist,
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",one\nhttp://tuner.invalid/one\n",
    )
    .expect("playlist");
    let (status, tuner) = send(
        &router,
        &auth_header,
        "POST",
        "/LiveTv/TunerHosts",
        Some(serde_json::json!({ "Type": "m3u", "Url": playlist.to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tuner added: {tuner}");
    let tuner_id = tuner["Id"].as_str().expect("tuner id").to_owned();
    let (listed, _) = live_tv_views(&router, &auth_header, &views_uri).await;
    assert!(listed, "a tuner added reaches the next request");

    let (status, body) = send(
        &router,
        &auth_header,
        "DELETE",
        &format!("/LiveTv/TunerHosts?id={tuner_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "tuner deleted: {body}");
    // The Live TV view's row stays behind, so the gate is asked by
    // `ensure_live_tv_view`, by the playlists probe's scope and by
    // `without_disabled_live_tv`: one answer serves all three.
    let (listed, reads) = live_tv_views(&router, &auth_header, &views_uri).await;
    assert!(!listed, "a tuner removed reaches the next request");
    assert_eq!(
        reads.len(),
        1,
        "the delete dropped the cached answer, which is read once:\n{}",
        reads.join("\n")
    );
    let (listed, reads) = live_tv_views(&router, &auth_header, &views_uri).await;
    assert!(!listed);
    assert!(
        reads.is_empty(),
        "a warm request answers from memory:\n{}",
        reads.join("\n")
    );
}

/// `GET views_uri`, recorded inside [`LIVE_TV_SPAN`]: whether it lists the
/// Live TV view, and the statements it ran against the tuner hosts.
async fn live_tv_views(
    router: &axum::Router,
    auth_header: &str,
    views_uri: &str,
) -> (bool, Vec<String>) {
    clear_statements(LIVE_TV_SPAN);
    let (status, views) = send(router, auth_header, "GET", views_uri, None)
        .instrument(tracing::info_span!(LIVE_TV_SPAN))
        .await;
    assert_eq!(status, StatusCode::OK, "views: {views}");
    let listed = views["Items"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["CollectionType"] == "livetv"));
    let reads = statements_in(LIVE_TV_SPAN)
        .into_iter()
        .filter(|s| s.contains(r#""FerrofinLiveTvTunerHosts""#))
        .collect();
    (listed, reads)
}

/// A browse that names a parent reads the parent's row once: the parent
/// visibility check reads it whole (`SELECT *`), and nothing reads it again,
/// neither whole nor as the repository's view row (`Type`, `Data`, … and
/// whether it has linked children). Upstream's `GetItems` reads the parent
/// once (`GetParentItem`) and works from that item. Covered: the release
/// gate's runtime-sorted library browse, a library's own children, and a box
/// set's listing and recursive browse; a box set's recursive browse still
/// asks whether the set has linked children, the one thing its row does not
/// say.
#[tokio::test]
async fn a_browse_reads_its_parent_s_row_once() {
    record_statements();
    let harness = boot(&[
        "Film Alpha (1999)/Film Alpha (1999).mkv",
        "Film Beta (2004)/Film Beta (2004).mkv",
    ])
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let (status, views) = send(
        &router,
        &auth_header,
        "GET",
        &format!("/Users/{user_id}/Views"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "views: {views}");
    let movies = views["Items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["Name"] == "Movies"))
        .and_then(|item| item["Id"].as_str())
        .expect("the Movies view")
        .to_owned();
    let (status, films) = send(
        &router,
        &auth_header,
        "GET",
        &format!("/Items?userId={user_id}&recursive=true&includeItemTypes=Movie"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "films: {films}");
    let ids: Vec<&str> = films["Items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["Id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2, "{films}");
    let (status, set) = send(
        &router,
        &auth_header,
        "POST",
        &format!("/Collections?name=Pair&ids={}", ids.join(",")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "collection: {set}");
    let set = set["Id"].as_str().expect("the set's id").to_owned();

    // (request, its whole-row reads by id, its `LinkedChildren` probes, how
    // many items it answers)
    let browses = [
        (
            format!(
                "/Items?userId={user_id}&parentId={movies}&includeItemTypes=Movie\
                 &recursive=true&sortBy=Runtime,SortName&sortOrder=Ascending&limit=50"
            ),
            1,
            0,
            2,
        ),
        (
            format!("/Items?userId={user_id}&parentId={movies}"),
            1,
            0,
            2,
        ),
        // The set's listing also reads the user root, whose libraries bound
        // the linked members the user may see (`GetLinkedChildren(user)`).
        (format!("/Items?userId={user_id}&parentId={set}"), 2, 0, 2),
        // A recursive browse of the set still asks whether it has linked
        // children, which its row does not say.
        (
            format!("/Items?userId={user_id}&parentId={set}&recursive=true"),
            1,
            1,
            2,
        ),
    ];
    for (uri, whole_rows, probes, items) in browses {
        browse_reading_the_parent_once(&router, &auth_header, &uri, whole_rows, probes, items)
            .await;
    }
}

/// Sends `uri` warm, then again inside [`PARENT_SPAN`], and checks the
/// recorded request reads `whole_rows` rows whole by id, never the
/// repository's view row, and `probes` times whether a parent has linked
/// children, answering `items` items as the warm one does.
async fn browse_reading_the_parent_once(
    router: &axum::Router,
    auth_header: &str,
    uri: &str,
    whole_rows: usize,
    probes: usize,
    items: i64,
) {
    let (status, warm) = send(router, auth_header, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {warm}");
    assert_eq!(warm["TotalRecordCount"], items, "{uri}: {warm}");
    clear_statements(PARENT_SPAN);
    let (status, page) = send(router, auth_header, "GET", uri, None)
        .instrument(tracing::info_span!(PARENT_SPAN))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page, warm, "the recorded request answers as the warm one");
    let statements = statements_in(PARENT_SPAN);
    let listing = statements.join("\n");
    let count = |needle: &str| statements.iter().filter(|s| s.contains(needle)).count();
    assert_eq!(
        count(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1 AND "Id" <> ?2"#),
        whole_rows,
        "{uri}: the parent's row is read once:\n{listing}"
    );
    assert_eq!(
        count(r#"AS "_HasLinkedChildren" FROM "BaseItems" WHERE "Id" = ?1"#),
        0,
        "{uri}: the scope works from the row the handler read:\n{listing}"
    );
    assert_eq!(
        count(r#"SELECT EXISTS (SELECT 1 FROM "LinkedChildren" WHERE "ParentId" = ?1)"#),
        probes,
        "{uri}: linked children are looked up only where they scope the browse:\n{listing}"
    );
}

/// With the library set warm, a library disabled from the dashboard leaves
/// the user's scope on the very next request, and comes back when enabled:
/// the options write drops the cached set the scope now reads `Enabled` from.
async fn a_dashboard_toggle_reaches_the_next_request(
    harness: &Harness,
    router: &axum::Router,
    auth_header: &str,
    user_id: &str,
) {
    let views_uri = format!("/Users/{user_id}/Views");
    let movies_in_scope = async || {
        let (status, page) = send(
            router,
            auth_header,
            "GET",
            &format!("/Items?userId={user_id}&recursive=true&includeItemTypes=Movie&limit=0"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "browse: {page}");
        page["TotalRecordCount"].as_i64()
    };
    assert_eq!(movies_in_scope().await, Some(2));
    let (status, folders) = send(router, auth_header, "GET", "/Library/VirtualFolders", None).await;
    assert_eq!(status, StatusCode::OK, "{folders}");
    let movies = folders
        .as_array()
        .and_then(|folders| folders.iter().find(|folder| folder["Name"] == "Movies"))
        .expect("the Movies library")
        .clone();
    let library = uuid::Uuid::parse_str(movies["ItemId"].as_str().expect("ItemId")).expect("id");
    for (enabled, expected) in [(false, 0), (true, 2)] {
        // The views' visibility check loads the set again after a toggle.
        let (status, _) = send(router, auth_header, "GET", &views_uri, None).await;
        assert_eq!(status, StatusCode::OK);
        let set = harness
            .db
            .cached_libraries()
            .expect("the set is warm before the toggle");
        assert_eq!(
            set.options.get(&library).map(|options| options.enabled),
            Some(!enabled),
            "the set holds the value the toggle has to replace"
        );
        let mut options = movies["LibraryOptions"].clone();
        options["Enabled"] = serde_json::json!(enabled);
        let (status, body) = send(
            router,
            auth_header,
            "POST",
            "/Library/VirtualFolders/LibraryOptions",
            Some(serde_json::json!({"Id": movies["ItemId"], "LibraryOptions": options})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "options saved: {body}");
        assert_eq!(
            movies_in_scope().await,
            Some(expected),
            "Enabled = {enabled} reaches the next request"
        );
    }
}

#[tokio::test]
async fn a_playback_start_leaves_the_now_playing_item_to_the_first_read() {
    record_statements();
    let harness = boot(&["Film Alpha (1999)/Film Alpha (1999).mkv"]).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let (status, films) = send(
        &router,
        &auth_header,
        "GET",
        &format!("/Items?userId={user_id}&recursive=true&includeItemTypes=Movie"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "films: {films}");
    let film = films["Items"][0]["Id"]
        .as_str()
        .expect("the film")
        .to_owned();

    // The bench's playback screen body (gates/bench/screens.js).
    let report = serde_json::json!({
        "PositionTicks": 0, "ItemId": film, "MediaSourceId": film, "PlaySessionId": "ps-1",
        "PlayMethod": "DirectPlay", "CanSeek": true, "IsPaused": false, "IsMuted": false,
        "VolumeLevel": 100, "RepeatMode": "RepeatNone",
    });
    let start = playing_statements(
        &router,
        &auth_header,
        "POST",
        "/Sessions/Playing",
        Some(report),
    )
    .await;
    assert_eq!(
        build_reads(&start),
        Vec::<&str>::new(),
        "the start report builds no NowPlayingItem: {start:#?}"
    );
    assert!(
        row_reads_by_id(&start) >= 1,
        "the report reads the item's row: {start:#?}"
    );

    // The first read builds it: the media source's streams and the item's
    // own tables, from the row the report read, so the row is not read again.
    let first = playing_statements(&router, &auth_header, "GET", "/Sessions", None).await;
    assert_eq!(
        build_reads(&first),
        [
            "AttachmentStreamInfos",
            "BaseItemImageInfos",
            "Chapters",
            "MediaStreamInfos",
            "TrickplayInfos"
        ],
        "{first:#?}"
    );
    assert_eq!(
        row_reads_by_id(&first),
        0,
        "the item's row is not read again: {first:#?}"
    );

    // A later read copies the item it built.
    let second = playing_statements(&router, &auth_header, "GET", "/Sessions", None).await;
    assert_eq!(build_reads(&second), Vec::<&str>::new(), "{second:#?}");
    let (status, sessions) = send(&router, &auth_header, "GET", "/Sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let shown = sessions
        .as_array()
        .and_then(|list| list.iter().find_map(|s| s["NowPlayingItem"]["Id"].as_str()))
        .expect("a session shows the film");
    assert_eq!(shown, film);
}

/// Sends a request inside [`PLAYING_SPAN`] and returns its statements.
async fn playing_statements(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> Vec<String> {
    clear_statements(PLAYING_SPAN);
    let (status, response) = send(router, auth_header, method, uri, body)
        .instrument(tracing::info_span!(PLAYING_SPAN))
        .await;
    assert!(status.is_success(), "{method} {uri}: {status} {response}");
    statements_in(PLAYING_SPAN)
}

/// The tables a `NowPlayingItem` build reads that nothing else in these
/// requests does, sorted.
fn build_reads(statements: &[String]) -> Vec<&'static str> {
    let tables = [
        "MediaStreamInfos",
        "AttachmentStreamInfos",
        "BaseItemImageInfos",
        "Chapters",
        "TrickplayInfos",
    ];
    statements
        .iter()
        .filter_map(|statement| {
            tables
                .into_iter()
                .find(|table| statement.starts_with(&format!(r#"SELECT * FROM "{table}""#)))
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// How many of `statements` read one item's whole row by id
/// (`get_item_by_id`, `retrieve_item`).
fn row_reads_by_id(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|statement| statement.contains(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#))
        .count()
}

/// `GET /Sessions` asks whether the caller is an administrator and may control
/// other users' sessions, and with `controllableByUserId` whether that user
/// shares device control. Upstream reads all three from the `User` it holds
/// (`SessionManager.GetSessions`); here they come from the policy cache, so a
/// warm listing reads no `Permissions` or `Preferences` row and reads the
/// caller's own row once, the `controllableByUserId` naming the caller
/// included. A policy saved through the API still reaches the very next
/// listing.
#[tokio::test]
async fn a_sessions_listing_reads_permissions_from_the_policy_cache() {
    record_statements();
    let harness = boot(&[]).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (admin, _) = sign_in(&router, ADMIN_USER, ADMIN_PASSWORD, "rig-admin").await;
    let (status, created) = send(
        &router,
        &admin,
        "POST",
        "/Users/New",
        Some(serde_json::json!({ "Name": "viewer", "Password": "viewer-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "user created: {created}");
    let (viewer, viewer_id) = sign_in(&router, "viewer", "viewer-pw", "rig-viewer").await;
    let devices = |sessions: &serde_json::Value| -> Vec<String> {
        let mut ids: Vec<String> = sessions
            .as_array()
            .expect("a session list")
            .iter()
            .filter_map(|session| session["DeviceId"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        ids
    };

    // Warm: the first listings leave both callers' policies cached.
    for header in [&admin, &viewer] {
        let (status, sessions) = send(&router, header, "GET", "/Sessions", None).await;
        assert_eq!(status, StatusCode::OK, "{sessions}");
    }
    let controllable = format!("/Sessions?controllableByUserId={viewer_id}");
    for (header, uri, expected) in [
        (&admin, "/Sessions", &["rig-admin", "rig-viewer"][..]),
        (&viewer, "/Sessions", &["rig-viewer"][..]),
        // No session here has a live connection, so none is controllable.
        (&viewer, controllable.as_str(), &[][..]),
    ] {
        clear_statements(SESSIONS_SPAN);
        let (status, sessions) = send(&router, header, "GET", uri, None)
            .instrument(tracing::info_span!(SESSIONS_SPAN))
            .await;
        assert_eq!(status, StatusCode::OK, "{uri}: {sessions}");
        assert_eq!(devices(&sessions), expected, "{uri}: {sessions}");
        let statements = statements_in(SESSIONS_SPAN);
        assert!(
            !statements
                .iter()
                .any(|s| s.contains(r#"FROM "Permissions""#) || s.contains(r#"FROM "Preferences""#)),
            "{uri} reads the policy from the cache: {statements:#?}"
        );
        assert_eq!(
            statements
                .iter()
                .filter(|s| s.contains(r#"FROM "Users""#))
                .count(),
            1,
            "{uri} reads the caller's row once: {statements:#?}"
        );
    }

    // The viewer made an administrator sees every session on the next
    // listing, and only its own again once that is taken back.
    let (status, user) = send(&router, &admin, "GET", &format!("/Users/{viewer_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{user}");
    let mut policy = user["Policy"].clone();
    for (is_admin, expected) in [
        (true, &["rig-admin", "rig-viewer"][..]),
        (false, &["rig-viewer"][..]),
    ] {
        policy["IsAdministrator"] = serde_json::json!(is_admin);
        let (status, body) = send(
            &router,
            &admin,
            "POST",
            &format!("/Users/{viewer_id}/Policy"),
            Some(policy.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "policy saved: {body}");
        let (status, sessions) = send(&router, &viewer, "GET", "/Sessions", None).await;
        assert_eq!(status, StatusCode::OK, "{sessions}");
        assert_eq!(
            devices(&sessions),
            expected,
            "IsAdministrator = {is_admin} reaches the next listing"
        );
    }
}

/// The user's views on a server holding the Live TV view and the user's
/// playlists view (gl-1322). Upstream's `GetNamedView` finds each row with
/// `GetItemById`, from its item cache. Here the listing of view rows the read
/// takes holds both, so a warm read looks neither up by id. A row deleted
/// since is made again on the next read, as `GetNamedView` makes it.
#[tokio::test]
async fn a_warm_views_read_finds_its_view_rows_in_its_listing() {
    record_statements();
    let harness = boot(&["Film Alpha (1999)/Film Alpha (1999).mkv"]).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    add_tuner_and_playlist(&harness, &router, &auth_header, &user_id).await;
    let views_uri = format!("/Users/{user_id}/Views");
    let view_ids = |views: &serde_json::Value| -> Vec<(String, String)> {
        views["Items"]
            .as_array()
            .expect("views")
            .iter()
            .map(|view| {
                (
                    view["CollectionType"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    view["Id"].as_str().expect("view id").to_owned(),
                )
            })
            .collect()
    };

    // Cold: the first read makes both view rows.
    let (status, cold) = send(&router, &auth_header, "GET", &views_uri, None).await;
    assert_eq!(status, StatusCode::OK, "views: {cold}");
    let listed = view_ids(&cold);
    let kinds: Vec<&str> = listed.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds, ["livetv", "movies", "playlists"], "{cold}");

    let user = uuid::Uuid::parse_str(&user_id).expect("user id");
    clear_statements(VIEW_ROWS_SPAN);
    let views = harness
        .wired
        .state
        .user_views
        .get_user_views(user)
        .instrument(tracing::info_span!(VIEW_ROWS_SPAN))
        .await
        .expect("views");
    assert_eq!(views.len(), 3, "{views:?}");
    let statements = statements_in(VIEW_ROWS_SPAN);
    let listing = statements.join("\n");
    assert!(
        !statements
            .iter()
            .any(|s| s.contains(r#"FROM "BaseItems" WHERE "Id" = ?1"#)),
        "a warm read looks no view row up by id:\n{listing}"
    );
    // It finds both rows, so it writes neither and lists the views once.
    assert!(
        !statements
            .iter()
            .any(|s| s.contains(r#"INSERT INTO "BaseItems""#)),
        "a warm read writes no view row:\n{listing}"
    );
    assert_eq!(
        statements
            .iter()
            .filter(|s| s.contains(r#"ORDER BY bi."SortName" ASC, bi."Name" ASC"#))
            .count(),
        1,
        "a warm read lists the view rows once:\n{listing}"
    );

    // Each row deleted is made again by the next read, under the same id.
    for (kind, id) in listed.iter().filter(|(kind, _)| kind != "movies") {
        let id = uuid::Uuid::parse_str(id).expect("view id");
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(id))
            .execute(harness.db.writer())
            .await
            .expect("view row deleted");
        let (status, views) = send(&router, &auth_header, "GET", &views_uri, None).await;
        assert_eq!(status, StatusCode::OK, "views: {views}");
        assert_eq!(view_ids(&views), listed, "the {kind} view is back: {views}");
    }
}

/// Adds an M3U tuner, so the Live TV view is listed, and a playlist of the
/// library's movie owned by `user_id`, so the user's Playlists view is.
async fn add_tuner_and_playlist(
    harness: &Harness,
    router: &axum::Router,
    auth_header: &str,
    user_id: &str,
) {
    let tuner = std::path::Path::new(&harness.movies_dir).with_file_name("tuner.m3u");
    std::fs::write(
        &tuner,
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",one\nhttp://tuner.invalid/one\n",
    )
    .expect("tuner playlist");
    let (status, body) = send(
        router,
        auth_header,
        "POST",
        "/LiveTv/TunerHosts",
        Some(serde_json::json!({ "Type": "m3u", "Url": tuner.to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tuner added: {body}");
    let (status, movies) = send(
        router,
        auth_header,
        "GET",
        &format!("/Items?userId={user_id}&includeItemTypes=Movie&recursive=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "movies: {movies}");
    let movie = movies["Items"][0]["Id"]
        .as_str()
        .expect("a movie")
        .to_owned();
    let (status, body) = send(
        router,
        auth_header,
        "POST",
        "/Playlists",
        Some(serde_json::json!({ "Name": "Mine", "Ids": [movie], "UserId": user_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "playlist created: {body}");
}

/// A page showing provider ids, a movie's own page, and a page showing only
/// its links each read the page's `BaseItemProviders` rows once (gl-1323).
/// The DTO's `ProviderIds` and the key of the user data the user never wrote
/// come from the same rows. The key takes a provider id only where upstream's
/// row was read with them (`.Include(e => e.Provider)`: `ApplyNavigations` for
/// `ItemFields.ProviderIds`, and `RetrieveItem`). So `ExternalUrls` alone reads
/// them for the links, and the key stays the movie's id.
#[tokio::test]
async fn an_item_page_reads_its_provider_ids_once() {
    record_statements();
    let harness = boot(&["Heat (1995) [tmdbid-949]/Heat (1995).mkv"]).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let list = |fields: &str| {
        format!("/Items?userId={user_id}&recursive=true&includeItemTypes=Movie&fields={fields}")
    };
    let (status, films) = send(&router, &auth_header, "GET", &list("ProviderIds"), None).await;
    assert_eq!(status, StatusCode::OK, "films: {films}");
    let film = films["Items"][0]["Id"]
        .as_str()
        .expect("the film")
        .to_owned();
    // `Guid.ToString()`: the key a never-written row is keyed by without
    // provider ids.
    let film_key = uuid::Uuid::parse_str(&film).expect("guid").to_string();

    for (uri, tmdb, key) in [
        (list("ProviderIds"), Some("949"), "949"),
        (
            format!("/Items/{film}?userId={user_id}"),
            Some("949"),
            "949",
        ),
        (list("ExternalUrls"), None, film_key.as_str()),
    ] {
        clear_statements(PROVIDERS_SPAN);
        let (status, body) = send(&router, &auth_header, "GET", &uri, None)
            .instrument(tracing::info_span!(PROVIDERS_SPAN))
            .await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let dto = body.get("Items").map_or(&body, |items| &items[0]);
        assert_eq!(dto["Id"], film.as_str(), "{uri}: {body}");
        assert_eq!(dto["ProviderIds"]["Tmdb"].as_str(), tmdb, "{uri}: {body}");
        assert_eq!(dto["UserData"]["Key"], key, "{uri}: {body}");
        let statements = statements_in(PROVIDERS_SPAN);
        assert_eq!(
            statements
                .iter()
                .filter(|s| s.contains(r#"FROM "BaseItemProviders""#))
                .count(),
            1,
            "{uri} reads the provider ids once: {statements:#?}"
        );
    }
}

/// A series' episodes and seasons with `ExternalUrls` read the series'
/// provider ids, which their links are built from, in the same
/// `BaseItemProviders` query as their own (gl-1390): with or without user
/// data, with `ProviderIds`, on the episode's own page, and on a page that
/// lists the series too. The user-data key still derives from the series'
/// ids, as without the field.
#[tokio::test]
async fn an_episode_page_reads_its_series_provider_ids_with_its_own() {
    record_statements();
    let harness = boot(&["Heat (1995)/Heat (1995).mkv"]).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = sign_in_and_scan(&harness, &router).await;
    let series = scan_a_show(&harness, &router, &auth_header, &user_id).await;
    let series = series.as_str();
    let shows = |path: &str, query: &str| format!("/Shows/{series}/{path}?userId={user_id}{query}");
    let (status, plain) = send(&router, &auth_header, "GET", &shows("Episodes", ""), None).await;
    assert_eq!(status, StatusCode::OK, "episodes: {plain}");
    let episode = plain["Items"][0]["Id"].as_str().expect("the episode");
    // The series' Tvdb id, then the season's (and the episode's) number.
    let episode_key = Some("81189001001");
    assert_eq!(plain["Items"][0]["UserData"]["Key"].as_str(), episode_key);
    let episode_link = "https://www.themoviedb.org/tv/1396/season/1/episode/1";
    let season_link = "https://www.themoviedb.org/tv/1396/season/1";
    let mixed = |order: &str| {
        format!(
            "/Items?userId={user_id}&recursive=true&includeItemTypes=Series,Episode\
             &fields=ProviderIds,ExternalUrls&sortBy=SortName&sortOrder={order}"
        )
    };

    // (uri, the kind checked, its link, its key). `ProviderIds` and the
    // item's own page hand the page's rows, the series' with them, to the
    // user-data key; a page holding the series too projects it first in one
    // of the two orders.
    for (uri, kind, link, key) in [
        (
            shows("Episodes", "&fields=ExternalUrls"),
            "Episode",
            episode_link,
            episode_key,
        ),
        (
            shows("Episodes", "&fields=ExternalUrls&enableUserData=false"),
            "Episode",
            episode_link,
            None,
        ),
        (
            shows("Episodes", "&fields=ProviderIds,ExternalUrls"),
            "Episode",
            episode_link,
            episode_key,
        ),
        (
            format!("/Items/{episode}?userId={user_id}"),
            "Episode",
            episode_link,
            episode_key,
        ),
        (
            shows("Seasons", "&fields=ExternalUrls"),
            "Season",
            season_link,
            Some("81189001"),
        ),
        (mixed("Ascending"), "Episode", episode_link, episode_key),
        (mixed("Descending"), "Episode", episode_link, episode_key),
    ] {
        clear_statements(SERIES_PROVIDERS_SPAN);
        let (status, body) = send(&router, &auth_header, "GET", &uri, None)
            .instrument(tracing::info_span!(SERIES_PROVIDERS_SPAN))
            .await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let dtos = body["Items"]
            .as_array()
            .map_or_else(|| vec![&body], |items| items.iter().collect());
        let dto = dtos
            .iter()
            .find(|dto| dto["Type"] == kind)
            .unwrap_or_else(|| panic!("{uri}: a {kind}: {body}"));
        assert!(
            dto["ExternalUrls"]
                .as_array()
                .is_some_and(|links| links.iter().any(|found| found["Url"] == link)),
            "{uri}: the series' Tmdb id builds the link: {body}"
        );
        assert_eq!(dto["UserData"]["Key"].as_str(), key, "{uri}: {body}");
        if uri.contains("includeItemTypes=Series") {
            let listed = dtos
                .iter()
                .find(|dto| dto["Type"] == "Series")
                .unwrap_or_else(|| panic!("{uri}: the series is listed: {body}"));
            assert_eq!(listed["ProviderIds"]["Tmdb"], "1396", "{uri}: {body}");
        }
        let statements = statements_in(SERIES_PROVIDERS_SPAN);
        assert_eq!(
            statements
                .iter()
                .filter(|s| s.contains(r#"FROM "BaseItemProviders""#))
                .count(),
            1,
            "{uri} runs one BaseItemProviders IN-query: {statements:#?}"
        );
    }
}
