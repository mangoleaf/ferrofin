//! Every credential form Jellyfin accepts, over the real composition root.
//!
//! The authentication path copies only the headers the authorization parser
//! reads, so this pins the two against each other: a form the copy dropped
//! would stop authenticating here. It runs each form with
//! `EnableLegacyAuthorization` off and on, beside the headers a browser sends
//! (`AuthorizationContext.cs`, `GetAuthorizationDictionary` and
//! `GetAuthorizationInfoFromDictionary`).

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request, StatusCode, header};
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "credential-forms-pw";
/// What a client names itself with, beside its token.
const IDENTITY: &str = r#"Client="test", Device="d", DeviceId="forms", Version="1""#;
/// Sent with every probe: a browser's ordinary request, none of them a
/// credential, however much they look like one.
const BROWSER_HEADERS: [(&str, &str); 8] = [
    ("host", "media.example.org"),
    ("user-agent", "Mozilla/5.0"),
    ("accept", "application/json"),
    ("accept-language", "sv,en;q=0.8"),
    ("cookie", "session=abc"),
    ("referer", "http://media.example.org/web/"),
    ("x-forwarded-for", "203.0.113.9"),
    ("x-real-ip", "203.0.113.9"),
];

/// One way to present `token`: the headers and the query string.
struct Form {
    label: &'static str,
    headers: Vec<(&'static str, String)>,
    query: Option<String>,
}

/// The forms that authenticate whatever `EnableLegacyAuthorization` says.
fn modern_forms(token: &str) -> Vec<Form> {
    vec![
        Form {
            label: "Authorization: MediaBrowser",
            headers: vec![(
                "authorization",
                format!(r#"MediaBrowser Token="{token}", {IDENTITY}"#),
            )],
            query: None,
        },
        Form {
            label: "?ApiKey=",
            headers: Vec::new(),
            query: Some(format!("ApiKey={token}")),
        },
        Form {
            label: "?apikey=, in another case",
            headers: Vec::new(),
            query: Some(format!("apikey={token}")),
        },
    ]
}

/// The Emby-era forms, honoured only with `EnableLegacyAuthorization` on.
fn legacy_forms(token: &str) -> Vec<Form> {
    vec![
        Form {
            label: "X-Emby-Authorization",
            headers: vec![(
                "x-emby-authorization",
                format!(r#"MediaBrowser Token="{token}", {IDENTITY}"#),
            )],
            query: None,
        },
        Form {
            label: "Authorization: Emby",
            headers: vec![(
                "authorization",
                format!(r#"Emby Token="{token}", {IDENTITY}"#),
            )],
            query: None,
        },
        Form {
            label: "X-Emby-Token",
            headers: vec![("x-emby-token", token.to_owned())],
            query: None,
        },
        Form {
            label: "X-MediaBrowser-Token",
            headers: vec![("x-mediabrowser-token", token.to_owned())],
            query: None,
        },
        Form {
            label: "?api_key=",
            headers: Vec::new(),
            query: Some(format!("api_key={token}")),
        },
    ]
}

/// A booted composition root and the temp dir keeping its database alive.
struct Harness {
    wired: WiredApp,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the administrator.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    for dir in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(dir)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-credential-forms".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
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
    Harness { wired, _temp: temp }
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// `GET {path}` the way `form` says, among a browser's headers.
async fn probe(router: &axum::Router, path: &str, form: &Form) -> axum::response::Response {
    let uri = form
        .query
        .as_ref()
        .map_or_else(|| path.to_owned(), |query| format!("{path}?{query}"));
    let mut request = Request::builder().uri(uri);
    for (name, value) in BROWSER_HEADERS {
        request = request.header(name, value);
    }
    for (name, value) in &form.headers {
        request = request.header(*name, value);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

/// Whether `/Users/Me` reports the administrator for `form`.
async fn assert_signed_in(router: &axum::Router, form: &Form) {
    let response = probe(router, "/Users/Me", form).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", form.label);
    assert_eq!(
        json_body(response).await["Name"],
        ADMIN_USER,
        "{}",
        form.label
    );
}

async fn assert_refused(router: &axum::Router, form: &Form) {
    let status = probe(router, "/Users/Me", form).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", form.label);
}

/// Signs the administrator in with the plain `MediaBrowser` header.
async fn sign_in(router: &axum::Router) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(header::AUTHORIZATION, format!("MediaBrowser {IDENTITY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
        ))
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    json_body(response).await["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned()
}

/// Turns `EnableLegacyAuthorization` on as an administrator would.
async fn enable_legacy_authorization(router: &axum::Router, token: &str) {
    let authorization = format!(r#"MediaBrowser Token="{token}", {IDENTITY}"#);
    let read = Request::builder()
        .uri("/System/Configuration")
        .header(header::AUTHORIZATION, &authorization)
        .body(Body::empty())
        .expect("request");
    let mut config = json_body(router.clone().oneshot(read).await.expect("response")).await;
    config["EnableLegacyAuthorization"] = serde_json::Value::Bool(true);
    let save = Request::builder()
        .method("POST")
        .uri("/System/Configuration")
        .header(header::AUTHORIZATION, &authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(config.to_string()))
        .expect("request");
    let status = router
        .clone()
        .oneshot(save)
        .await
        .expect("response")
        .status();
    assert_eq!(status, StatusCode::NO_CONTENT, "configuration saved");
}

/// Legacy authorization off, its default: only the modern forms authenticate.
#[tokio::test]
async fn the_modern_forms_authenticate_and_the_legacy_ones_do_not() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let token = sign_in(&router).await;

    for form in modern_forms(&token) {
        assert_signed_in(&router, &form).await;
    }
    for form in legacy_forms(&token) {
        assert_refused(&router, &form).await;
    }
    for form in modern_forms("not-a-token") {
        assert_refused(&router, &form).await;
    }
    let anonymous = Form {
        label: "no credentials",
        headers: Vec::new(),
        query: None,
    };
    assert_refused(&router, &anonymous).await;
}

/// Legacy authorization on: every form authenticates, and a wrong token in any
/// of them is still refused.
#[tokio::test]
async fn every_form_authenticates_with_legacy_authorization_on() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let token = sign_in(&router).await;
    enable_legacy_authorization(&router, &token).await;

    for form in modern_forms(&token).iter().chain(&legacy_forms(&token)) {
        assert_signed_in(&router, form).await;
    }
    for form in modern_forms("not-a-token")
        .iter()
        .chain(&legacy_forms("not-a-token"))
    {
        assert_refused(&router, form).await;
    }
}

/// An API key authenticates through the same forms as a device token.
#[tokio::test]
async fn an_api_key_authenticates_through_the_header_and_the_query() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let token = sign_in(&router).await;
    let authorization = format!(r#"MediaBrowser Token="{token}", {IDENTITY}"#);
    let create = Request::builder()
        .method("POST")
        .uri("/Auth/Keys?app=forms")
        .header(header::AUTHORIZATION, &authorization)
        .body(Body::empty())
        .expect("request");
    let status = router
        .clone()
        .oneshot(create)
        .await
        .expect("response")
        .status();
    assert_eq!(status, StatusCode::NO_CONTENT);
    let list = Request::builder()
        .uri("/Auth/Keys")
        .header(header::AUTHORIZATION, &authorization)
        .body(Body::empty())
        .expect("request");
    let keys = json_body(router.clone().oneshot(list).await.expect("response")).await;
    let key = keys["Items"][0]["AccessToken"]
        .as_str()
        .expect("AccessToken");

    for form in modern_forms(key) {
        // `/Users` is administrator-only, which an API key is.
        let status = probe(&router, "/Users", &form).await.status();
        assert_eq!(status, StatusCode::OK, "{}", form.label);
    }
}

/// The header values the parser picks among are the ones it always saw: the
/// first readable `Authorization`, and past an empty `X-Emby-Token` the next
/// header in upstream's order.
#[tokio::test]
async fn repeated_and_empty_credential_headers_keep_their_order() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let token = sign_in(&router).await;
    enable_legacy_authorization(&router, &token).await;
    let valid = format!(r#"MediaBrowser Token="{token}", {IDENTITY}"#);

    // A value that is not text is skipped; the one after it is read.
    let mut request = Request::builder()
        .uri("/Users/Me")
        .body(Body::empty())
        .expect("request");
    let headers = request.headers_mut();
    headers.append(
        header::AUTHORIZATION,
        HeaderValue::from_bytes(&[0xff, 0xfe]).expect("opaque bytes are a header value"),
    );
    headers.append(
        header::AUTHORIZATION,
        HeaderValue::from_str(&valid).expect("header"),
    );
    let status = router
        .clone()
        .oneshot(request)
        .await
        .expect("response")
        .status();
    assert_eq!(status, StatusCode::OK, "the second Authorization is read");

    // Two readable values: the first decides, and it is wrong.
    let mut request = Request::builder()
        .uri("/Users/Me")
        .body(Body::empty())
        .expect("request");
    let headers = request.headers_mut();
    headers.append(
        header::AUTHORIZATION,
        HeaderValue::from_static(r#"MediaBrowser Token="not-a-token""#),
    );
    headers.append(
        header::AUTHORIZATION,
        HeaderValue::from_str(&valid).expect("header"),
    );
    let status = router
        .clone()
        .oneshot(request)
        .await
        .expect("response")
        .status();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the first decides");

    // `string.IsNullOrEmpty(token)` moves on to the next source.
    let mut request = Request::builder()
        .uri("/Users/Me")
        .body(Body::empty())
        .expect("request");
    let headers = request.headers_mut();
    headers.insert(
        HeaderName::from_static("x-emby-token"),
        HeaderValue::from_static(""),
    );
    headers.insert(
        HeaderName::from_static("x-mediabrowser-token"),
        HeaderValue::from_str(&token).expect("header"),
    );
    let status = router
        .clone()
        .oneshot(request)
        .await
        .expect("response")
        .status();
    assert_eq!(status, StatusCode::OK, "an empty header is no token");
}
