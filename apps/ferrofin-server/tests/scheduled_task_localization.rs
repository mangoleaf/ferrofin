//! `GET /ScheduledTasks` names and descriptions in the request's culture, and
//! formatted with the live configuration, end to end.
//!
//! Boots the real composition root, so the request localization, the
//! administrator gate, the handler and the core task registry with the real
//! localizer all take part. Upstream's core tasks return
//! `_localization.GetLocalizedString("<key>")`, which since Jellyfin 12 reads
//! the request's `CurrentUICulture`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "tasks-pw";
const IDENT: &str = r#"Client="test", Device="d", DeviceId="tasks", Version="1""#;

/// Boots the composition root over a temp database.
async fn boot(temp: &tempfile::TempDir) -> WiredApp {
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-task-localization".to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let db = ferrofin_db::Database::connect(&config.database_url())
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
    wired
}

/// Sends one request, asserting `expected`, and returns its JSON body
/// (`null` when it has none).
async fn send(
    router: &axum::Router,
    request: axum::http::request::Builder,
    body: Body,
    expected: StatusCode,
) -> serde_json::Value {
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), expected);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    }
}

/// [`send`] for a request answered `200`.
async fn get_json(
    router: &axum::Router,
    request: axum::http::request::Builder,
    body: Body,
) -> serde_json::Value {
    send(router, request, body, StatusCode::OK).await
}

/// The administrator's `Authorization` header value.
fn bearer(token: &str) -> String {
    format!(r#"MediaBrowser Token="{token}", {IDENT}"#)
}

/// Signs the seeded administrator in and returns the access token.
async fn login(router: &axum::Router) -> String {
    let login = get_json(
        router,
        Request::post("/Users/AuthenticateByName")
            .header(header::AUTHORIZATION, format!("MediaBrowser {IDENT}"))
            .header(header::CONTENT_TYPE, "application/json"),
        Body::from(serde_json::json!({ "Username": "admin", "Pw": ADMIN_PASSWORD }).to_string()),
    )
    .await;
    login["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned()
}

/// `GET /ScheduledTasks` as the administrator, with `language` as the
/// request's `Accept-Language` when given, keyed by task key.
async fn tasks(
    router: &axum::Router,
    token: &str,
    language: Option<&str>,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut request = Request::get("/ScheduledTasks").header(header::AUTHORIZATION, bearer(token));
    if let Some(language) = language {
        request = request.header(header::ACCEPT_LANGUAGE, language);
    }
    let list = get_json(router, request, Body::empty()).await;
    list.as_array()
        .expect("a task list")
        .iter()
        .map(|task| (task["Key"].as_str().expect("Key").to_owned(), task.clone()))
        .collect()
}

#[tokio::test]
async fn scheduled_tasks_answer_in_the_request_culture() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(&temp).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = &login(&router).await;

    let english = tasks(&router, token, None).await;
    let task = |tasks: &std::collections::HashMap<String, serde_json::Value>, key: &str| {
        let task = &tasks[key];
        (
            task["Name"].as_str().expect("Name").to_owned(),
            task["Category"].as_str().expect("Category").to_owned(),
        )
    };
    assert_eq!(
        task(&english, "RefreshLibrary"),
        ("Scan Media Library".into(), "Library".into())
    );
    assert_eq!(
        task(&english, "CleanActivityLog"),
        ("Clean Activity Log".into(), "Maintenance".into())
    );
    // Jellyfin 12's keys (jellyfin/jellyfin#16488), not the raw 10.11 one.
    assert_eq!(
        task(&english, "RefreshInternetChannels"),
        ("Refresh Channels".into(), "Internet Channels".into())
    );
    // Upstream leaves this one a literal in every culture.
    assert_eq!(
        task(&english, "RefreshGuide"),
        ("Refresh Guide".into(), "Live TV".into())
    );

    let german = tasks(&router, token, Some("de")).await;
    assert_eq!(
        task(&german, "RefreshLibrary"),
        ("Medien-Bibliothek scannen".into(), "Bibliothek".into())
    );
    assert_eq!(
        task(&german, "CleanActivityLog"),
        ("Aktivitätsverlauf bereinigen".into(), "Wartung".into())
    );
    assert_eq!(
        task(&german, "RefreshGuide"),
        ("Refresh Guide".into(), "Live TV".into())
    );
    assert!(
        german["CleanLogFiles"]["Description"]
            .as_str()
            .expect("Description")
            .starts_with("Lösche Log-Dateien, die älter als "),
        "{}",
        german["CleanLogFiles"]
    );
    assert_eq!(
        german.len(),
        english.len(),
        "the same tasks in both cultures"
    );

    // Outside any request (a scheduled run, its result and its pushes) there
    // is no request culture: the server's live `UICulture` applies, which only
    // the composition root's localizer knows.
    let mut settings = get_json(
        &router,
        Request::get("/System/Configuration").header(header::AUTHORIZATION, bearer(token)),
        Body::empty(),
    )
    .await;
    settings["UICulture"] = serde_json::json!("de");
    send(
        &router,
        Request::post("/System/Configuration")
            .header(header::AUTHORIZATION, bearer(token))
            .header(header::CONTENT_TYPE, "application/json"),
        Body::from(settings.to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    let outside = wired.state.tasks.get_tasks().await.expect("tasks");
    let library = outside
        .iter()
        .find(|task| task.key.as_deref() == Some("RefreshLibrary"))
        .expect("RefreshLibrary");
    assert_eq!(library.name.as_deref(), Some("Medien-Bibliothek scannen"));
    assert_eq!(library.category.as_deref(), Some("Bibliothek"));

    for task in wired.background {
        task.abort();
    }
}

/// Upstream's `DeleteLogFileTask.Description` formats `TaskCleanLogsDescription`
/// with the live `LogFileRetentionDays` on every read, so a saved retention
/// shows at once, without a restart.
#[tokio::test]
async fn clean_log_directory_description_follows_the_saved_retention() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(&temp).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = &login(&router).await;
    let description = |tasks: &std::collections::HashMap<String, serde_json::Value>| {
        tasks["CleanLogFiles"]["Description"]
            .as_str()
            .expect("Description")
            .to_owned()
    };
    assert_eq!(
        description(&tasks(&router, token, None).await),
        "Deletes log files that are more than 3 days old."
    );

    let mut settings = get_json(
        &router,
        Request::get("/System/Configuration").header(header::AUTHORIZATION, bearer(token)),
        Body::empty(),
    )
    .await;
    settings["LogFileRetentionDays"] = serde_json::json!(9);
    send(
        &router,
        Request::post("/System/Configuration")
            .header(header::AUTHORIZATION, bearer(token))
            .header(header::CONTENT_TYPE, "application/json"),
        Body::from(settings.to_string()),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(
        description(&tasks(&router, token, None).await),
        "Deletes log files that are more than 9 days old."
    );

    for task in wired.background {
        task.abort();
    }
}
