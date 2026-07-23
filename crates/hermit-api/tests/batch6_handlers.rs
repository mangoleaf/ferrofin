//! Batch-6 handler success-path tests — Users admin, Startup, QuickConnect.
//!
//! Drives each newly-real handler through `create_router` with injected
//! `hermit-traits` fakes that authenticate and return canned data, asserting the
//! status and wire-body shape. Managers a given handler never touches reuse the
//! `test_support` panic fakes, so a stray call is caught.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAuthContext, FakeDto, FakeLibrary, FakeMediaSources, FakeMusic, FakeProviders, FakeSearch,
    FakeSimilarItems, FakeSystem, FakeUserData, FakeUserViews,
};
use hermit_db::entities::security::DeviceEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::{ServerConfiguration, UserConfiguration};
use hermit_model::dto::{NameIdPair, SessionInfoDto, UserDto};
use hermit_model::quick_connect::QuickConnectResult;
use hermit_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use hermit_model::users::UserPolicy;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::error::ServiceError;
use hermit_traits::library::UserManager;
use hermit_traits::net::{AuthService, RequestContext};
use hermit_traits::options::AuthorizationInfo;
use hermit_traits::security::QuickConnect;
use hermit_traits::session::{AuthenticationRequest, SessionManager};
use hermit_traits::system::ServerApplicationPaths;
use std::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_ID: Uuid = Uuid::from_u128(0x0AD1);
const BOB_ID: Uuid = Uuid::from_u128(0x0B0B);

/// Builds a neutral [`UserEntity`] with the given id + name (no `Default`).
fn user_entity(id: Uuid, username: &str, password: Option<&str>) -> UserEntity {
    UserEntity {
        id: id.to_string(),
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
        normalized_username: username.to_ascii_uppercase(),
        password: password.map(ToOwned::to_owned),
        password_reset_provider_id: String::new(),
        play_default_audio_track: false,
        remember_audio_selections: false,
        remember_subtitle_selections: false,
        remote_client_bitrate_limit: None,
        row_version: 0,
        subtitle_language_preference: None,
        subtitle_mode: 0,
        sync_play_access: 0,
        username: username.to_owned(),
    }
}

/// An [`AuthService`] authenticating as the admin user (so `RequireAuth` passes).
struct AdminAuth;

#[async_trait]
impl AuthService for AdminAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(ADMIN_ID, "admin", Some("hash"))),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`UserManager`] backed by an in-memory table, recording password/policy
/// writes so the tests can assert them.
#[derive(Default)]
struct MemUsers {
    /// Records the last (user_id, new_password) passed to `change_password`.
    changed_password: Mutex<Option<(Uuid, String)>>,
    /// Records the last policy passed to `update_policy`.
    updated_policy: Mutex<Option<(Uuid, UserPolicy)>>,
    /// Records whether `delete_user` was called and for whom.
    deleted: Mutex<Option<Uuid>>,
}

/// The fixed two-user table shared by [`MemUsers`] and the assertions.
fn mem_users() -> Vec<UserEntity> {
    vec![
        user_entity(ADMIN_ID, "admin", Some("hash")),
        user_entity(BOB_ID, "bob", None),
    ]
}

#[async_trait]
impl UserManager for MemUsers {
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        Ok(mem_users())
    }
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(vec![ADMIN_ID, BOB_ID])
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok(mem_users().into_iter().find(|u| u.id == id.to_string()))
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        Ok(Some(user_entity(BOB_ID, "bob", None)))
    }
    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserEntity>, ServiceError> {
        Ok(mem_users().into_iter().find(|u| u.username == name))
    }
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn create_user(&self, name: &str) -> Result<UserEntity, ServiceError> {
        Ok(user_entity(Uuid::from_u128(0x0E11), name, None))
    }
    async fn delete_user(&self, user_id: Uuid) -> Result<(), ServiceError> {
        *self.deleted.lock().unwrap() = Some(user_id);
        Ok(())
    }
    async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn change_password(&self, user_id: Uuid, new_password: &str) -> Result<(), ServiceError> {
        *self.changed_password.lock().unwrap() = Some((user_id, new_password.to_owned()));
        Ok(())
    }
    async fn authenticate_user(
        &self,
        _username: &str,
        _password: &str,
        _remote_endpoint: &str,
        _is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        Ok(Some(user_entity(ADMIN_ID, "admin", Some("hash"))))
    }
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_user_dto(
        &self,
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<UserDto, ServiceError> {
        // The admin user reports IsAdministrator; everyone else does not.
        let is_admin = user.id == ADMIN_ID.to_string();
        Ok(UserDto {
            id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
            name: Some(user.username.clone()),
            server_id,
            has_password: Some(user.password.is_some()),
            policy: Some(UserPolicy {
                is_administrator: is_admin,
                ..UserPolicy::default()
            }),
            configuration: Some(UserConfiguration::default()),
            ..UserDto::default()
        })
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn update_policy(&self, user_id: Uuid, policy: &UserPolicy) -> Result<(), ServiceError> {
        *self.updated_policy.lock().unwrap() = Some((user_id, policy.clone()));
        Ok(())
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// A [`ServerConfigurationManager`] over an in-memory [`ServerConfiguration`],
/// recording the last persisted value.
struct MemConfig {
    /// The current configuration, mutated by `update_configuration`.
    config: Mutex<ServerConfiguration>,
}

impl MemConfig {
    fn new(wizard_done: bool) -> Self {
        let c = ServerConfiguration {
            is_startup_wizard_completed: wizard_done,
            server_name: "Old".to_owned(),
            ..ServerConfiguration::default()
        };
        Self {
            config: Mutex::new(c),
        }
    }
}

#[async_trait]
impl ServerConfigurationManager for MemConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(hermit_api::test_support::FakePaths)
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(self.config.lock().unwrap().clone())
    }
    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        *self.config.lock().unwrap() = configuration.clone();
        Ok(())
    }
    async fn get_branding(&self) -> Result<hermit_model::branding::BrandingOptions, ServiceError> {
        Ok(hermit_model::branding::BrandingOptions::default())
    }
    async fn update_branding(
        &self,
        _branding: &hermit_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// A [`QuickConnect`] returning canned pairing data.
struct OkQuickConnect;

#[async_trait]
impl QuickConnect for OkQuickConnect {
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn try_connect(
        &self,
        _authorization_info: &AuthorizationInfo,
    ) -> Result<QuickConnectResult, ServiceError> {
        Ok(QuickConnectResult {
            secret: "sec".to_owned(),
            code: "123456".to_owned(),
            ..QuickConnectResult::default()
        })
    }
    async fn check_request_status(&self, secret: &str) -> Result<QuickConnectResult, ServiceError> {
        Ok(QuickConnectResult {
            secret: secret.to_owned(),
            authenticated: true,
            ..QuickConnectResult::default()
        })
    }
    async fn authorize_request(&self, _user_id: Uuid, _code: &str) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_authorized_request(&self, _secret: &str) -> Result<SessionInfoDto, ServiceError> {
        Ok(SessionInfoDto {
            user_id: ADMIN_ID,
            user_name: Some("admin".to_owned()),
            ..SessionInfoDto::default()
        })
    }
}

/// A [`SessionManager`] whose `revoke_user_tokens` no-ops (used by delete/policy);
/// every other method is unused by these tests.
struct NoopSessions;

#[async_trait]
impl SessionManager for NoopSessions {
    async fn log_session_activity(
        &self,
        _app_name: &str,
        _app_version: &str,
        _device_id: &str,
        _device_name: &str,
        _remote_endpoint: &str,
        _user: &UserEntity,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_device_name(
        &self,
        _session_id: &str,
        _reported_device_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_start(&self, _info: &PlaybackStartInfo) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_progress(
        &self,
        _info: &PlaybackProgressInfo,
        _is_automated: bool,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_stopped(&self, _info: &PlaybackStopInfo) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_session_ended(&self, _session_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_general_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &GeneralCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &MessageCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_play_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &PlayRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_playstate_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &PlaystateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_admin_sessions(
        &self,
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_user_sessions(
        &self,
        _user_ids: &[Uuid],
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_user_device_sessions(
        &self,
        _device_id: &str,
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn add_additional_user(
        &self,
        _session_id: &str,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_additional_user(
        &self,
        _session_id: &str,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_now_viewing_item(
        &self,
        _session_id: &str,
        _item_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_new_session(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_direct(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn report_capabilities(
        &self,
        _session_id: &str,
        _capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_transcoding_info(
        &self,
        _device_id: &str,
        _info: &TranscodingInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn clear_transcoding_info(&self, _device_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_sessions(
        &self,
        _user_id: Uuid,
        _device_id: Option<&str>,
        _active_within_seconds: Option<i32>,
        _controllable_user_to_check: Option<Uuid>,
        _is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_session_by_authentication_token(
        &self,
        _token: &str,
        _device_id: &str,
        _remote_endpoint: &str,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn logout(&self, _access_token: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn logout_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn revoke_user_tokens(
        &self,
        _user_id: Uuid,
        _current_access_token: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn close_live_stream_if_needed(
        &self,
        _live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// Assembles an [`AppState`] from the batch-6 fakes plus panic fakes elsewhere.
fn state(users: Arc<MemUsers>, config: Arc<MemConfig>) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        users,
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(NoopSessions),
        Arc::new(FakeSystem),
        Arc::new(hermit_api::test_support::FakeAppHost),
        config,
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(AdminAuth),
        Arc::new(OkQuickConnect),
        Arc::new(hermit_api::test_support::FakePlaylists),
        Arc::new(hermit_api::test_support::FakeCollections),
        Arc::new(hermit_api::test_support::FakeTvSeries),
        Arc::new(hermit_api::test_support::FakeSubtitles),
        Arc::new(hermit_api::test_support::FakeLyrics),
        Arc::new(hermit_api::test_support::FakeMediaSegments),
        Arc::new(hermit_api::test_support::FakeTrickplay),
        Arc::new(hermit_api::test_support::FakeDevices),
        Arc::new(hermit_api::test_support::FakeClientEventLogger),
        Arc::new(hermit_api::test_support::FakeApiKeys),
        Arc::new(hermit_api::test_support::FakeLocalization),
        Arc::new(hermit_api::test_support::FakeDisplayPreferences),
        Arc::new(hermit_api::test_support::FakeActivity),
        Arc::new(hermit_api::test_support::FakeFileSystem),
        Arc::new(hermit_api::test_support::FakeTasks),
    )
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn get_users_returns_all_sorted() {
    let router = create_router(state(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Users")
                .header("X-Emby-Token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Sorted by username: admin before bob.
    assert_eq!(arr[0]["Name"], "admin");
    assert_eq!(arr[1]["Name"], "bob");
}

#[tokio::test]
async fn get_user_by_id_found_and_missing() {
    let router = create_router(state(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
    ));
    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Users/{BOB_ID}"))
                .header("X-Emby-Token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let json = body_json(ok).await;
    assert_eq!(json["Name"], "bob");

    let missing = router
        .oneshot(
            Request::builder()
                .uri(format!("/Users/{}", Uuid::from_u128(0xDEAD)))
                .header("X-Emby-Token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_user_records_deletion() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Users/{BOB_ID}"))
                .header("X-Emby-Token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(*users.deleted.lock().unwrap(), Some(BOB_ID));
}

#[tokio::test]
async fn update_policy_records_and_guards_last_admin() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));

    // Demoting the sole admin is forbidden.
    let forbidden = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{ADMIN_ID}/Policy"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&UserPolicy::default()).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    // Updating bob's policy is fine and is recorded.
    let policy = UserPolicy {
        max_active_sessions: 5,
        ..UserPolicy::default()
    };
    let ok = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{BOB_ID}/Policy"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&policy).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);
    let recorded = users.updated_policy.lock().unwrap().clone().unwrap();
    assert_eq!(recorded.0, BOB_ID);
    assert_eq!(recorded.1.max_active_sessions, 5);
}

#[tokio::test]
async fn quick_connect_enabled_and_initiate_and_connect() {
    let router = create_router(state(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
    ));

    let enabled = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/QuickConnect/Enabled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    assert_eq!(body_json(enabled).await, serde_json::json!(true));

    let initiate = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/QuickConnect/Initiate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(initiate.status(), StatusCode::OK);
    assert_eq!(body_json(initiate).await["Code"], "123456");

    let connect = router
        .oneshot(
            Request::builder()
                .uri("/QuickConnect/Connect?secret=abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(connect.status(), StatusCode::OK);
    let json = body_json(connect).await;
    assert_eq!(json["Secret"], "abc");
    assert_eq!(json["Authenticated"], true);
}

#[tokio::test]
async fn quick_connect_authorize_requires_auth() {
    let router = create_router(state(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/QuickConnect/Authorize?code=123456")
                .header("X-Emby-Token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, serde_json::json!(true));
}

#[tokio::test]
async fn startup_configuration_round_trip_and_complete() {
    let config = Arc::new(MemConfig::new(false));
    let router = create_router(state(Arc::new(MemUsers::default()), config.clone()));

    // GET reflects the current server name.
    let got = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Startup/Configuration")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    assert_eq!(body_json(got).await["ServerName"], "Old");

    // POST updates it.
    let update = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Startup/Configuration")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "ServerName": "New", "UICulture": "en-GB" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update.status(), StatusCode::NO_CONTENT);
    assert_eq!(config.config.lock().unwrap().server_name, "New");

    // Complete flips the wizard flag.
    let complete = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Startup/Complete")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(complete.status(), StatusCode::NO_CONTENT);
    assert!(config.config.lock().unwrap().is_startup_wizard_completed);
}

#[tokio::test]
async fn startup_user_get_and_set_password() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(false))));

    let got = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Startup/User")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    assert_eq!(body_json(got).await["Name"], "bob");

    // Setting the first user's password records the change.
    let set = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Startup/User")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "Name": "bob", "Password": "hunter2" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(set.status(), StatusCode::NO_CONTENT);
    let (id, pw) = users.changed_password.lock().unwrap().clone().unwrap();
    assert_eq!(id, BOB_ID);
    assert_eq!(pw, "hunter2");
}

#[tokio::test]
async fn forgot_password_reports_contact_admin() {
    let router = create_router(state(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/ForgotPassword")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "EnteredUsername": "bob" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["Action"], "ContactAdmin");
}
