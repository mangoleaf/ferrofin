//! The statements `GET /Users/{id}/Views` runs, end to end (gl-981).
//!
//! The home screen's view row asks three questions about the libraries'
//! physical folders: the scope of 12.2's `HasVisibleItem` probe for the
//! playlists folder, that probe's own query, and the views' `ChildCount`
//! (`GetChildCountSourceIds`). All three read `CollectionFolder` rows, which
//! the library set the visibility check caches already holds, `Data`
//! included. The probe's scope also looks for the Live TV view among the
//! `UserView` rows it has just read. So a warm request reads neither again.
//!
//! Boots the real composition root over a temp data dir with one scanned
//! movies library and the playlists folder every server provisions, and
//! records, through a global subscriber, the `sqlx::query` events emitted
//! inside one request's span (the SQLite worker enters the caller's span).
//! Then, the set still warm, it toggles the library's `Enabled` option through
//! the dashboard's endpoint and checks the user's scope follows on the next
//! request.

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
const ADMIN_PASSWORD: &str = "user-views-statements-pw";

const CLIENT_AUTH: &str = r#"MediaBrowser Client="Views Statements", Device="Test Rig", DeviceId="rig-vs", Version="1.0.0""#;

/// The span the recorded request runs in.
const REQUEST_SPAN: &str = "gl981_views_request";

/// The statements recorded inside [`REQUEST_SPAN`], in order.
static STATEMENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Records the text of every `sqlx::query` event emitted inside [`REQUEST_SPAN`].
struct Statements;

impl<S> tracing_subscriber::Layer<S> for Statements
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let in_request = ctx
            .event_scope(event)
            .is_some_and(|mut scope| scope.any(|span| span.name() == REQUEST_SPAN));
        if !in_request {
            return;
        }
        let mut text = StatementText(String::new());
        event.record(&mut text);
        STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text.0.split_whitespace().collect::<Vec<_>>().join(" "));
    }
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

/// Boots the composition root over a temp DB and seeds the admin.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-user-views-statements".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let movies_dir = temp.path().join("movies");
    for title in ["Film Alpha (1999)", "Film Beta (2004)"] {
        let dir = movies_dir.join(title);
        std::fs::create_dir_all(&dir).expect("movie dir");
        std::fs::write(dir.join(format!("{title}.mkv")), [0u8; 1024]).expect("movie file");
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

#[tokio::test]
async fn a_warm_views_request_reads_no_library_row_again() {
    record_statements();
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (status, auth) = send(
        &router,
        CLIENT_AUTH,
        "POST",
        "/Users/AuthenticateByName",
        Some(serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin authenticates: {auth}");
    let token = auth["AccessToken"].as_str().expect("token");
    let user_id = auth["User"]["Id"].as_str().expect("user id").to_owned();
    let auth_header = format!(
        r#"MediaBrowser Client="Views Statements", Device="Test Rig", DeviceId="rig-vs", Version="1.0.0", Token="{token}""#
    );

    // No remote metadata or image fetchers: a scan that asks TMDb waits on it.
    let (status, body) = send(
        &router,
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
    STATEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let (status, views) = send(&router, &auth_header, "GET", &views_uri, None)
        .instrument(tracing::info_span!(REQUEST_SPAN))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(views, warm, "the recorded request answers as the warm one");
    assert_eq!(movies_count(&views), Some(2), "{views}");
    let statements = STATEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
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
