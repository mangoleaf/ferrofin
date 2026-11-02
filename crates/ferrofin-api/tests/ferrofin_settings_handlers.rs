//! Handler tests for the Ferrofin settings page's route
//! (`handlers::ferrofin_settings`).
//!
//! Each test drives `GET /Ferrofin/Settings` or `POST /Ferrofin/Settings`
//! through the real router against a [`FakeSettings`] manager, asserting the
//! status, the body and whether the manager was reached at all. The caller is an API key
//! (`authed_state_with_plugins`), a plain user (`user_authed_state_with_plugins`)
//! or nobody (`fake_state`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_plugins, fake_state, user_authed_state_with_plugins,
};
use ferrofin_model::server_settings::{
    ServerSettingApplies, ServerSettingDto, ServerSettingGroup, ServerSettingKind,
    ServerSettingSource, ServerSettingsDto, ServerSettingsUpdateDto, ServerSettingsUpdateResultDto,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::DisabledPluginManager;
use ferrofin_traits::server_settings::ServerSettingsManager;
use tower::ServiceExt;

/// The secret the fake reports, which only an administrator may receive.
const SECRET: &str = "omdb-key-only-admins-see";

/// What [`FakeSettings`] answers.
enum Answer {
    Settings,
    Disabled,
    FileNoLongerLoads,
    /// `update` refuses the change; `snapshot` reports the settings.
    ChangeRefused,
    /// `update` cannot write `config.toml`; `snapshot` reports the settings.
    Unwritable,
}

/// A [`ServerSettingsManager`] fake that counts its calls and keeps the last
/// update it was given.
struct FakeSettings {
    answer: Answer,
    calls: AtomicUsize,
    update: Mutex<Option<ServerSettingsUpdateDto>>,
}

impl FakeSettings {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: AtomicUsize::new(0),
            update: Mutex::new(None),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_update(&self) -> Option<ServerSettingsUpdateDto> {
        self.update.lock().expect("update lock").clone()
    }
}

/// What [`FakeSettings::update`] answers when it saves.
fn saved() -> ServerSettingsUpdateResultDto {
    ServerSettingsUpdateResultDto {
        changed: vec!["log_level".to_owned(), "scan_probe_concurrency".to_owned()],
        restart_required: true,
        process_restart_required: vec!["log_level".to_owned()],
    }
}

fn settings() -> ServerSettingsDto {
    ServerSettingsDto {
        config_file: "/srv/ferrofin/config.toml".to_owned(),
        config_file_writable: true,
        restart_pending: false,
        settings: vec![
            ServerSettingDto {
                key: "omdb_api_key".to_owned(),
                group: ServerSettingGroup::Providers,
                kind: ServerSettingKind::Secret,
                description: "OMDb API key override.".to_owned(),
                env_var: Some("FERROFIN_OMDB_KEY".to_owned()),
                flag: None,
                default: Some("Jellyfin's shared key".to_owned()),
                value: Some(serde_json::json!(SECRET)),
                saved_value: None,
                source: ServerSettingSource::ConfigFile,
                editable: true,
                locked_reason: None,
                applies: ServerSettingApplies::Restart,
                min: None,
                max: None,
            },
            ServerSettingDto {
                key: "port".to_owned(),
                group: ServerSettingGroup::Network,
                kind: ServerSettingKind::Integer,
                description: "HTTP port.".to_owned(),
                env_var: Some("FERROFIN_PORT".to_owned()),
                flag: Some("--port".to_owned()),
                default: Some("8096".to_owned()),
                value: Some(serde_json::json!(8097)),
                saved_value: None,
                source: ServerSettingSource::Flag,
                editable: false,
                locked_reason: Some("Set by the `--port` command-line flag.".to_owned()),
                applies: ServerSettingApplies::Restart,
                min: Some(0),
                max: Some(65535),
            },
        ],
    }
}

#[async_trait::async_trait]
impl ServerSettingsManager for FakeSettings {
    async fn snapshot(&self) -> Result<ServerSettingsDto, ServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.answer {
            Answer::Settings | Answer::ChangeRefused | Answer::Unwritable => Ok(settings()),
            Answer::Disabled => Err(ServiceError::not_found("the Ferrofin plugin is disabled")),
            Answer::FileNoLongerLoads => Err(ServiceError::conflict(
                "config file `/srv/ferrofin/config.toml` no longer loads",
            )),
        }
    }

    async fn update(
        &self,
        update: ServerSettingsUpdateDto,
    ) -> Result<ServerSettingsUpdateResultDto, ServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.update.lock().expect("update lock") = Some(update);
        match self.answer {
            Answer::Settings => Ok(saved()),
            Answer::Disabled => Err(ServiceError::not_found("the Ferrofin plugin is disabled")),
            Answer::FileNoLongerLoads => Err(ServiceError::conflict(
                "`/srv/ferrofin/config.toml` changed since the server started and no longer loads",
            )),
            Answer::ChangeRefused => Err(ServiceError::invalid_input(
                "`omdb_api_key` cannot be changed here: Set by the `FERROFIN_OMDB_KEY` \
                 environment variable.",
            )),
            Answer::Unwritable => Err(ServiceError::conflict(
                "`/srv/ferrofin/config.toml` cannot be written: Permission denied (os error 13)",
            )),
        }
    }
}

fn get_settings() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/Ferrofin/Settings")
        .header("Authorization", "Token abc")
        .body(Body::empty())
        .expect("request")
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

fn as_admin(fake: Arc<FakeSettings>) -> AppState {
    authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_server_settings(fake)
}

#[tokio::test]
async fn an_admin_gets_every_setting_secrets_included() {
    let fake = FakeSettings::new(Answer::Settings);
    let resp = create_router(as_admin(fake.clone()))
        .oneshot(get_settings())
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(resp).await).expect("json body");
    assert_eq!(
        body,
        serde_json::to_value(settings()).expect("serialize"),
        "the manager's snapshot, as it is"
    );
    assert_eq!(body["ConfigFile"], "/srv/ferrofin/config.toml");
    assert_eq!(body["Settings"][0]["Value"], SECRET);
    assert_eq!(body["Settings"][1]["Source"], "Flag");
    assert_eq!(body["Settings"][1]["Editable"], false);
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn a_plain_user_is_refused_before_the_settings_are_read() {
    let fake = FakeSettings::new(Answer::Settings);
    let state = user_authed_state_with_plugins(Arc::new(DisabledPluginManager))
        .with_server_settings(fake.clone());
    let resp = create_router(state)
        .oneshot(get_settings())
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_string(resp).await;
    assert!(!body.contains(SECRET), "{body}");
    assert_eq!(fake.calls(), 0, "a non-admin never reaches the manager");
}

#[tokio::test]
async fn an_anonymous_caller_is_unauthorized() {
    let fake = FakeSettings::new(Answer::Settings);
    let resp = create_router(fake_state().with_server_settings(fake.clone()))
        .oneshot(
            Request::builder()
                .uri("/Ferrofin/Settings")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn a_disabled_plugin_answers_not_found() {
    let fake = FakeSettings::new(Answer::Disabled);
    let resp = create_router(as_admin(fake))
        .oneshot(get_settings())
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_server_with_no_settings_manager_answers_not_found() {
    let state = authed_state_with_plugins(Arc::new(DisabledPluginManager));
    let resp = create_router(state)
        .oneshot(get_settings())
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_config_file_that_no_longer_loads_is_a_conflict_naming_it() {
    let fake = FakeSettings::new(Answer::FileNoLongerLoads);
    let resp = create_router(as_admin(fake))
        .oneshot(get_settings())
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_string(resp).await;
    assert!(body.contains("/srv/ferrofin/config.toml"), "{body}");
}

// ---- POST /Ferrofin/Settings ---------------------------------------------------

fn post_settings(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/Ferrofin/Settings")
        .header("Authorization", "Token abc")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

/// A save naming a secret, so a test can check it never comes back.
const SAVE: &str = r#"{"Changes":{"scan_probe_concurrency":4,"log_level":null,"omdb_api_key":"omdb-key-only-admins-see"}}"#;

#[tokio::test]
async fn an_admin_saves_changes_and_gets_what_was_saved() {
    let fake = FakeSettings::new(Answer::Settings);
    let resp = create_router(as_admin(fake.clone()))
        .oneshot(post_settings(SAVE))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(resp).await).expect("json body");
    assert_eq!(
        body,
        serde_json::json!({
            "Changed": ["log_level", "scan_probe_concurrency"],
            "RestartRequired": true,
            "ProcessRestartRequired": ["log_level"]
        })
    );
    let update = fake.last_update().expect("the manager got the update");
    assert_eq!(
        update.changes.get("scan_probe_concurrency"),
        Some(&Some(serde_json::json!(4)))
    );
    assert_eq!(update.changes.get("log_level"), Some(&None), "null removes");
    assert_eq!(
        update.changes.get("omdb_api_key"),
        Some(&Some(serde_json::json!(SECRET)))
    );
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn a_plain_user_is_refused_before_anything_is_saved() {
    let fake = FakeSettings::new(Answer::Settings);
    let state = user_authed_state_with_plugins(Arc::new(DisabledPluginManager))
        .with_server_settings(fake.clone());
    let resp = create_router(state)
        .oneshot(post_settings(SAVE))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(fake.calls(), 0, "a non-admin never reaches the manager");
}

#[tokio::test]
async fn an_anonymous_save_is_unauthorized() {
    let fake = FakeSettings::new(Answer::Settings);
    let resp = create_router(fake_state().with_server_settings(fake.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Ferrofin/Settings")
                .header("Content-Type", "application/json")
                .body(Body::from(SAVE))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn a_save_while_the_plugin_is_disabled_answers_not_found() {
    let fake = FakeSettings::new(Answer::Disabled);
    let resp = create_router(as_admin(fake))
        .oneshot(post_settings(SAVE))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let unwired = authed_state_with_plugins(Arc::new(DisabledPluginManager));
    let resp = create_router(unwired)
        .oneshot(post_settings(SAVE))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no settings manager");
}

#[tokio::test]
async fn a_refused_change_is_a_bad_request_that_quotes_no_value() {
    let fake = FakeSettings::new(Answer::ChangeRefused);
    let resp = create_router(as_admin(fake))
        .oneshot(post_settings(SAVE))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(resp).await).expect("json body");
    let error = body["error"].as_str().expect("an error message");
    assert!(error.contains("omdb_api_key"), "{error}");
    assert!(error.contains("FERROFIN_OMDB_KEY"), "{error}");
    assert!(!body.to_string().contains(SECRET), "{body}");
}

#[tokio::test]
async fn an_unwritable_config_file_is_a_conflict_naming_it() {
    for answer in [Answer::Unwritable, Answer::FileNoLongerLoads] {
        let fake = FakeSettings::new(answer);
        let resp = create_router(as_admin(fake))
            .oneshot(post_settings(SAVE))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_string(resp).await;
        assert!(body.contains("/srv/ferrofin/config.toml"), "{body}");
        assert!(!body.contains(SECRET), "{body}");
    }
}

#[tokio::test]
async fn a_body_that_is_no_update_never_reaches_the_manager() {
    for body in [
        r#"{"Changes":[]}"#,
        "[]",
        "{",
        r#"{"Changes":{"db_pool":4}"#,
        "{}",
    ] {
        let fake = FakeSettings::new(Answer::Settings);
        let resp = create_router(as_admin(fake.clone()))
            .oneshot(post_settings(body))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(fake.calls(), 0, "{body}");
    }
    let fake = FakeSettings::new(Answer::Settings);
    let resp = create_router(as_admin(fake.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Ferrofin/Settings")
                .header("Authorization", "Token abc")
                .header("Content-Type", "text/plain")
                .body(Body::from(SAVE))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(fake.calls(), 0);
}
