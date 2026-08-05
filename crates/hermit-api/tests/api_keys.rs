//! API keys handler tests: list, create, and revoke auth keys.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate and return canned data. Managers
//! a given handler never touches reuse the `test_support` panic fakes, so a
//! handler that strays trips a panic.
//!
//! This file carries the shared batch-12 harness (auth stub + `state()` builder
//! wiring every manager); the Devices/ClientLog/Config stubs the harness defines
//! are exercised only by their own domain files, so they are `dead_code` here.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAppHost, FakeClientEventLogger, FakeCollections, FakeConfig, FakeDevices, FakeDto,
    FakeLibrary, FakeLyrics, FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists,
    FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem,
    FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews,
};
use hermit_db::entities::security::{DeviceEntity, DeviceOptionsEntity};
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::ServerConfiguration;
use hermit_model::dto::DeviceInfoDto;
use hermit_model::querying::QueryResult;
use hermit_model::security::AuthenticationInfo;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::devices::{DeviceManager, DeviceQuery};
use hermit_traits::error::ServiceError;
use hermit_traits::events::ClientEventLogger;
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::AuthorizationInfo;
use hermit_traits::security::ApiKeyManager;
use hermit_traits::system::ServerApplicationPaths;
use std::sync::Arc as StdArc;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00C1_0000);

/// A minimal authenticated user.
fn user() -> UserEntity {
    UserEntity {
        id: USER_ID.to_string(),
        audio_language_preference: None,
        authentication_provider_id: String::new(),
        cast_receiver_id: None,
        display_collections_view: false,
        display_missing_episodes: false,
        enable_auto_login: false,
        enable_local_password: false,
        enable_next_episode_auto_play: false,
        enable_user_preference_access: false,
        hide_played_in_latest: false,
        internal_id: 0,
        invalid_login_attempt_count: 0,
        last_activity_date: None,
        last_login_date: None,
        login_attempts_before_lockout: None,
        max_active_sessions: 0,
        max_parental_rating_score: None,
        max_parental_rating_sub_score: None,
        must_update_password: false,
        normalized_username: String::new(),
        password: None,
        password_reset_provider_id: String::new(),
        play_default_audio_track: false,
        remember_audio_selections: false,
        remember_subtitle_selections: false,
        remote_client_bitrate_limit: None,
        row_version: 0,
        subtitle_language_preference: None,
        subtitle_mode: 0,
        sync_play_access: 0,
        username: "bob".to_owned(),
    }
}

/// An auth stub that authenticates as [`USER_ID`], carrying client/version so the
/// client-log handler can build a filename.
struct OkAuth {
    is_api_key: bool,
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            client: Some("Test Client".to_owned()),
            version: Some("9.9.9".to_owned()),
            is_api_key: self.is_api_key,
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

/// A [`DeviceManager`] with canned devices/options, capturing option updates.
#[derive(Default)]
struct StubDevices {
    /// Device id → info returned by `get_device`/`get_devices_for_user`.
    known: Vec<DeviceInfoDto>,
    /// Device id → options row returned by `get_device_options`.
    options: Option<DeviceOptionsEntity>,
    /// Captured `(device_id, custom_name)` update calls.
    updates: Mutex<Vec<(String, Option<String>)>>,
}

#[async_trait]
impl DeviceManager for StubDevices {
    async fn create_device(&self, _device: &DeviceEntity) -> Result<DeviceEntity, ServiceError> {
        unimplemented!()
    }
    async fn save_capabilities(
        &self,
        _device_id: &str,
        _capabilities: &hermit_model::session::ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_capabilities(
        &self,
        _device_id: Option<&str>,
    ) -> Result<hermit_model::session::ClientCapabilities, ServiceError> {
        unimplemented!()
    }
    async fn get_device(&self, id: &str) -> Result<Option<DeviceInfoDto>, ServiceError> {
        Ok(self
            .known
            .iter()
            .find(|d| d.id.as_deref() == Some(id))
            .cloned())
    }
    async fn get_devices(
        &self,
        _query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceEntity>, ServiceError> {
        // No live sessions to log out — the delete handler's per-device loop is a
        // no-op (so the panic `FakeSessions.logout_device` is never reached).
        Ok(QueryResult::from_items(Vec::new()))
    }
    async fn get_device_infos(
        &self,
        _query: &DeviceQuery,
    ) -> Result<QueryResult<hermit_model::devices::DeviceInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_devices_for_user(
        &self,
        _user_id: Option<Uuid>,
    ) -> Result<QueryResult<DeviceInfoDto>, ServiceError> {
        Ok(QueryResult::from_items(self.known.clone()))
    }
    async fn delete_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn can_access_device(
        &self,
        _user: &UserEntity,
        _device_id: &str,
    ) -> Result<bool, ServiceError> {
        unimplemented!()
    }
    async fn update_device_options(
        &self,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.updates
            .lock()
            .unwrap()
            .push((device_id.to_owned(), device_name.map(ToOwned::to_owned)));
        Ok(())
    }
    async fn get_device_options(
        &self,
        _device_id: &str,
    ) -> Result<Option<DeviceOptionsEntity>, ServiceError> {
        Ok(self.options.clone())
    }
    async fn to_client_capabilities_dto(
        &self,
        _capabilities: &hermit_model::session::ClientCapabilities,
    ) -> Result<hermit_model::dto::ClientCapabilitiesDto, ServiceError> {
        unimplemented!()
    }
}

/// An [`ApiKeyManager`] over an in-memory list, capturing create/delete calls.
#[derive(Default)]
struct StubApiKeys {
    keys: Mutex<Vec<AuthenticationInfo>>,
    created: Mutex<Vec<String>>,
    deleted: Mutex<Vec<String>>,
}

#[async_trait]
impl ApiKeyManager for StubApiKeys {
    async fn get_api_keys(&self) -> Result<Vec<AuthenticationInfo>, ServiceError> {
        Ok(self.keys.lock().unwrap().clone())
    }
    async fn create_api_key(&self, name: &str) -> Result<(), ServiceError> {
        self.created.lock().unwrap().push(name.to_owned());
        Ok(())
    }
    async fn delete_api_key(&self, access_token: &str) -> Result<(), ServiceError> {
        self.deleted.lock().unwrap().push(access_token.to_owned());
        Ok(())
    }
}

/// A [`ClientEventLogger`] capturing the write call and returning a fixed name.
#[derive(Default)]
struct StubClientLog {
    calls: Mutex<Vec<(String, String, Vec<u8>)>>,
}

#[async_trait]
impl ClientEventLogger for StubClientLog {
    async fn write_document(
        &self,
        client_name: &str,
        client_version: &str,
        contents: &[u8],
    ) -> Result<String, ServiceError> {
        self.calls.lock().unwrap().push((
            client_name.to_owned(),
            client_version.to_owned(),
            contents.to_vec(),
        ));
        Ok("upload_saved.log".to_owned())
    }
}

/// A [`ServerConfigurationManager`] returning a configuration with the
/// client-log upload flag set as requested.
struct StubConfig {
    allow_upload: bool,
}

#[async_trait]
impl ServerConfigurationManager for StubConfig {
    fn application_paths(&self) -> StdArc<dyn ServerApplicationPaths> {
        unimplemented!()
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(ServerConfiguration {
            allow_client_log_upload: self.allow_upload,
            ..Default::default()
        })
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_branding(&self) -> Result<hermit_model::branding::BrandingOptions, ServiceError> {
        unimplemented!()
    }
    async fn update_branding(
        &self,
        _branding: &hermit_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// Builds an [`AppState`] over the batch-12 managers, defaulting the rest to the
/// shared panic fakes.
#[allow(clippy::too_many_arguments)]
fn state(
    auth: Arc<OkAuth>,
    devices: Arc<dyn DeviceManager>,
    api_keys: Arc<dyn ApiKeyManager>,
    client_log: Arc<dyn ClientEventLogger>,
    config: Arc<dyn ServerConfigurationManager>,
) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(hermit_api::test_support::FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        config,
        Arc::new(hermit_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        auth.clone(),
        auth,
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        devices,
        client_log,
        api_keys,
        Arc::new(hermit_api::test_support::FakeLocalization),
        Arc::new(hermit_api::test_support::FakeDisplayPreferences),
        Arc::new(hermit_api::test_support::FakeActivity),
        Arc::new(hermit_api::test_support::FakeFileSystem),
        Arc::new(hermit_api::test_support::FakeTasks),
    )
}

/// A device info DTO with the given id.
fn device(id: &str) -> DeviceInfoDto {
    DeviceInfoDto {
        id: Some(id.to_owned()),
        name: Some("Phone".to_owned()),
        ..Default::default()
    }
}

/// Sends an authenticated request and returns `(status, body-bytes)`.
async fn call(app: AppState, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    call_with_body(app, method, uri, Body::empty(), None).await
}

async fn call_with_body(
    app: AppState,
    method: &str,
    uri: &str,
    body: Body,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", "tok");
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    let response = create_router(app)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn ok_auth() -> Arc<OkAuth> {
    Arc::new(OkAuth { is_api_key: false })
}
// ---- ApiKeys ---------------------------------------------------------------

#[tokio::test]
async fn get_keys_wraps_in_query_result() {
    let keys = Arc::new(StubApiKeys::default());
    keys.keys.lock().unwrap().push(AuthenticationInfo {
        app_name: Some("cli".to_owned()),
        access_token: Some("abc".into()),
        ..Default::default()
    });
    let app = state(
        ok_auth(),
        Arc::new(FakeDevices),
        keys,
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeConfig),
    );
    let (status, body) = call(app, "GET", "/Auth/Keys").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["TotalRecordCount"], 1);
    assert_eq!(json["Items"][0]["AppName"], "cli");
    assert_eq!(json["Items"][0]["AccessToken"], "abc");
}

#[tokio::test]
async fn create_key_passes_app_name() {
    let keys = Arc::new(StubApiKeys::default());
    let captured = keys.clone();
    let app = state(
        ok_auth(),
        Arc::new(FakeDevices),
        keys,
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeConfig),
    );
    let (status, _) = call(app, "POST", "/Auth/Keys?app=my-app").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(captured.created.lock().unwrap().as_slice(), &["my-app"]);
}

#[tokio::test]
async fn create_key_missing_app_is_bad_request() {
    let app = state(
        ok_auth(),
        Arc::new(FakeDevices),
        Arc::new(StubApiKeys::default()),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeConfig),
    );
    let (status, _) = call(app, "POST", "/Auth/Keys").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn revoke_key_passes_token() {
    let keys = Arc::new(StubApiKeys::default());
    let captured = keys.clone();
    let app = state(
        ok_auth(),
        Arc::new(FakeDevices),
        keys,
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeConfig),
    );
    let (status, _) = call(app, "DELETE", "/Auth/Keys/tok-123").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(captured.deleted.lock().unwrap().as_slice(), &["tok-123"]);
}
