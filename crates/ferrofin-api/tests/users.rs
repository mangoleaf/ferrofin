//! Users — CRUD, policy, forgot-password, AuthenticateByName, Users/Me.
//!
//! Drives the user-admin, account-recovery, and current-user handlers through
//! `create_router` with injected `ferrofin-traits` fakes that authenticate and
//! return canned data, asserting the status and wire-body shape. Two harnesses
//! coexist here: the batch6 `state(users, config)` harness (user CRUD / policy /
//! forgot-password) and the `handler_success_paths` `ok_state` harness
//! (AuthenticateByName / Users/Me). Their `user_entity` helpers clashed, so the
//! `ok_state` one is renamed `hsp_user_entity`; nothing else was changed.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAuthContext, FakeConfig, FakeDto, FakeLibrary, FakeMediaSources, FakeMusic, FakeProviders,
    FakeSearch, FakeSimilarItems, FakeSystem, FakeUserData, FakeUserViews, FixedAuth,
    fake_user_entity,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::configuration::{ServerConfiguration, UserConfiguration};
use ferrofin_model::dto::{BaseItemDto, MediaSourceInfo, NameIdPair, SessionInfoDto, UserDto};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::quick_connect::QuickConnectResult;
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use ferrofin_model::users::{
    ForgotPasswordAction, ForgotPasswordResult, PinRedeemResult, UserPolicy,
};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, MediaSourceManager, UserManager, UserViewManager};
use ferrofin_traits::net::{AuthService, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::security::QuickConnect;
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use ferrofin_traits::system::ServerApplicationPaths;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_ID: Uuid = Uuid::from_u128(0x0AD1);
const BOB_ID: Uuid = Uuid::from_u128(0x0B0B);
/// A user the store returns with a malformed (non-`Guid`) stored id.
const CORRUPT_ID: Uuid = Uuid::from_u128(0x0C0C);

/// Builds a neutral [`UserEntity`] with the given id + name (no `Default`).
fn user_entity(id: Uuid, username: &str, password: Option<&str>) -> UserEntity {
    UserEntity {
        password: password.map(ToOwned::to_owned),
        ..fake_user_entity(id, username)
    }
}

/// An [`AuthService`] authenticating as the admin user (so `RequireAuth` passes).
fn admin_auth() -> FixedAuth {
    FixedAuth(AuthorizationInfo {
        user: Some(Arc::new(user_entity(ADMIN_ID, "admin", Some("hash")))),
        token: Some("current-session".into()),
        is_authenticated: true,
        ..AuthorizationInfo::default()
    })
}

/// A [`UserManager`] backed by an in-memory table, recording password/policy
/// writes so the tests can assert them.
#[derive(Default)]
struct MemUsers {
    /// Records the last (user_id, new_password) passed to `change_password`.
    changed_password: Mutex<Option<(Uuid, String)>>,
    /// Records each (entered username, in local network) passed to
    /// `start_forgot_password_process`.
    forgot_requests: Mutex<Vec<(String, bool)>>,
    /// Records each PIN passed to `redeem_password_reset_pin`.
    redeemed_pins: Mutex<Vec<String>>,
    /// Records the last policy passed to `update_policy`.
    updated_policy: Mutex<Option<(Uuid, UserPolicy)>>,
    /// Records the last configuration passed to `update_configuration`.
    updated_configuration: Mutex<Option<(Uuid, UserConfiguration)>>,
    /// Records the last (user_id, new_name) passed to `rename_user`.
    renamed: Mutex<Option<(Uuid, String)>>,
    /// Records whether `delete_user` was called and for whom.
    deleted: Mutex<Option<Uuid>>,
    /// Makes `delete_user` refuse, as the last user or the last admin.
    refuse_delete: std::sync::atomic::AtomicBool,
    /// Makes `get_user_dto` answer the policy last saved for that user.
    echo_saved_policy: std::sync::atomic::AtomicBool,
    /// Listed by `get_users` before the two fixed accounts.
    listed_first: Option<UserEntity>,
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
        Ok(self
            .listed_first
            .iter()
            .cloned()
            .chain(mem_users())
            .collect())
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        // A row whose stored `Id` is not a `Guid` — the corrupt-database shape
        // `assert_can_update_user` must not mistake for the caller's own row.
        if id == CORRUPT_ID {
            let mut user = user_entity(CORRUPT_ID, "corrupt", None);
            "not-a-guid".clone_into(&mut user.id);
            return Ok(Some(user));
        }
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
        user_id: Uuid,
        _old_name: &str,
        new_name: &str,
    ) -> Result<(), ServiceError> {
        *self.renamed.lock().unwrap() = Some((user_id, new_name.to_owned()));
        Ok(())
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn create_user(&self, name: &str) -> Result<UserEntity, ServiceError> {
        Ok(user_entity(Uuid::from_u128(0x0E11), name, None))
    }
    async fn delete_user(&self, user_id: Uuid) -> Result<(), ServiceError> {
        if self.refuse_delete.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ServiceError::invalid_input(
                "there must be at least one admin user in the system",
            ));
        }
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
    async fn start_forgot_password_process(
        &self,
        entered_username: &str,
        is_in_network: bool,
    ) -> Result<ForgotPasswordResult, ServiceError> {
        self.forgot_requests
            .lock()
            .unwrap()
            .push((entered_username.to_owned(), is_in_network));
        Ok(ForgotPasswordResult {
            action: ForgotPasswordAction::PinCode,
            pin_file: Some("/data/passwordreset0123.json".to_owned()),
            pin_expiration_date: Some(
                chrono::DateTime::parse_from_rfc3339("2026-11-02T10:30:00Z")
                    .unwrap()
                    .into(),
            ),
        })
    }
    async fn redeem_password_reset_pin(&self, pin: &str) -> Result<PinRedeemResult, ServiceError> {
        self.redeemed_pins.lock().unwrap().push(pin.to_owned());
        if pin == "A1B2C3D4" {
            Ok(PinRedeemResult {
                success: true,
                users_reset: vec!["bob".to_owned()],
            })
        } else {
            Err(ServiceError::not_found(
                "no matching password reset request",
            ))
        }
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
        let saved = self
            .echo_saved_policy
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| self.updated_policy.lock().unwrap().clone())
            .flatten()
            .filter(|(id, _)| id.to_string() == user.id)
            .map(|(_, policy)| policy);
        Ok(UserDto {
            id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
            name: Some(user.username.clone()),
            server_id,
            policy: Some(saved.unwrap_or_else(|| UserPolicy {
                is_administrator: is_admin,
                // This account fixture has no device restrictions.
                enable_all_devices: true,
                ..UserPolicy::default()
            })),
            configuration: Some(UserConfiguration::default()),
            ..UserDto::default()
        })
    }
    async fn update_configuration(
        &self,
        user_id: Uuid,
        config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        *self.updated_configuration.lock().unwrap() = Some((user_id, config.clone()));
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
        Arc::new(ferrofin_api::test_support::FakePaths)
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(self.config.lock().unwrap().clone()))
    }
    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        *self.config.lock().unwrap() = configuration.clone();
        Ok(())
    }
    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
        Ok(ferrofin_model::branding::BrandingOptions::default())
    }
    async fn update_branding(
        &self,
        _branding: &ferrofin_model::branding::BrandingOptions,
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
            secret: "sec".into(),
            code: "123456".to_owned(),
            ..QuickConnectResult::default()
        })
    }
    async fn check_request_status(&self, secret: &str) -> Result<QuickConnectResult, ServiceError> {
        Ok(QuickConnectResult {
            secret: secret.into(),
            authenticated: true,
            ..QuickConnectResult::default()
        })
    }
    async fn authorize_request(&self, _user_id: Uuid, _code: &str) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_authorized_request(
        &self,
        _secret: &str,
    ) -> Result<AuthenticationResultData, ServiceError> {
        Ok(AuthenticationResultData {
            session: SessionInfoDto {
                user_id: ADMIN_ID,
                user_name: Some("admin".to_owned()),
                ..SessionInfoDto::default()
            },
            access_token: "qc-token".into(),
        })
    }
}

/// Records token revocation while leaving unrelated session methods unused.
#[derive(Default)]
struct NoopSessions {
    revoked: Mutex<Vec<(Uuid, String)>>,
}

#[async_trait]
impl SessionManager for NoopSessions {
    async fn log_session_activity(
        &self,
        _app_name: &str,
        _app_version: &str,
        _device_id: &str,
        _device_name: &str,
        _remote_endpoint: &str,
        _user: Option<&UserEntity>,
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
        // The delete handler pushes UserDeleted here best-effort. A rename's
        // or policy save's UserUpdated comes from the user manager, which is a
        // fake here (apps/ferrofin-server/tests/user_updated_push.rs).
        Ok(())
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
    ) -> Result<AuthenticationResultData, ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_direct(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
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
        user_id: Uuid,
        current_access_token: &str,
    ) -> Result<(), ServiceError> {
        self.revoked
            .lock()
            .unwrap()
            .push((user_id, current_access_token.to_owned()));
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
    state_with_activity(
        users,
        config,
        Arc::new(ferrofin_api::test_support::FakeActivity),
    )
}

/// [`state`] with a caller-supplied [`ActivityManager`] so tests can assert
/// what a handler wrote to the activity log.
fn state_with_activity(
    users: Arc<MemUsers>,
    config: Arc<MemConfig>,
    activity: Arc<dyn ferrofin_traits::activity::ActivityManager>,
) -> AppState {
    state_with_auth(users, config, activity, Arc::new(admin_auth()))
}

/// [`state_with_activity`] with a caller-supplied [`AuthService`], so a test can
/// drive a handler as an API-key caller (no user) rather than as the admin.
fn state_with_auth(
    users: Arc<MemUsers>,
    config: Arc<MemConfig>,
    activity: Arc<dyn ferrofin_traits::activity::ActivityManager>,
    auth: Arc<dyn AuthService>,
) -> AppState {
    state_with_sessions(
        users,
        config,
        activity,
        auth,
        Arc::new(NoopSessions::default()),
    )
}

fn state_with_sessions(
    users: Arc<MemUsers>,
    config: Arc<MemConfig>,
    activity: Arc<dyn ferrofin_traits::activity::ActivityManager>,
    auth: Arc<dyn AuthService>,
    sessions: Arc<dyn SessionManager>,
) -> AppState {
    state_with_playlists(
        users,
        config,
        activity,
        auth,
        sessions,
        Arc::new(ferrofin_api::test_support::FakePlaylists),
    )
}

/// [`state_with_sessions`] with a caller-supplied [`PlaylistManager`]
/// (`ferrofin_traits::collections::PlaylistManager`).
fn state_with_playlists(
    users: Arc<MemUsers>,
    config: Arc<MemConfig>,
    activity: Arc<dyn ferrofin_traits::activity::ActivityManager>,
    auth: Arc<dyn AuthService>,
    sessions: Arc<dyn SessionManager>,
    playlists: Arc<dyn ferrofin_traits::collections::PlaylistManager>,
) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        users,
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        sessions,
        Arc::new(FakeSystem),
        Arc::new(ferrofin_api::test_support::FakeAppHost),
        config,
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        auth,
        Arc::new(OkQuickConnect),
        playlists,
        Arc::new(ferrofin_api::test_support::FakeCollections),
        Arc::new(ferrofin_api::test_support::FakeTvSeries),
        Arc::new(ferrofin_api::test_support::FakeSubtitles),
        Arc::new(ferrofin_api::test_support::FakeLyrics),
        Arc::new(ferrofin_api::test_support::FakeMediaSegments),
        Arc::new(ferrofin_api::test_support::FakeTrickplay),
        Arc::new(ferrofin_api::test_support::FakeDevices),
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        activity,
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);

/// Builds a minimal [`UserEntity`] carrying the given id + username; every other
/// field is a neutral zero value ([`UserEntity`] has no `Default`).
fn hsp_user_entity(id: Uuid, username: &str) -> UserEntity {
    UserEntity {
        password: Some("hashed".to_owned()),
        ..fake_user_entity(id, username)
    }
}

/// An [`AuthService`] that always authenticates as [`USER_ID`], so `RequireAuth`
/// yields an authenticated context in the success-path tests.
fn ok_auth_service() -> FixedAuth {
    FixedAuth::signed_in(hsp_user_entity(USER_ID, "alice"))
}

/// An `AuthorizationContext` that resolves the same authenticated user, so
/// handlers reading the request extension (e.g. `AuthenticateByName`'s client
/// identity) see a populated context.
fn ok_auth_context() -> FixedAuth {
    FixedAuth(AuthorizationInfo {
        user: Some(Arc::new(hsp_user_entity(USER_ID, "alice"))),
        client: Some("Wolphin".to_owned()),
        version: Some("1.0".to_owned()),
        device_id: Some("dev-1".to_owned()),
        device: Some("Test Device".to_owned()),
        is_authenticated: true,
        ..AuthorizationInfo::default()
    })
}

/// A [`UserManager`] whose `get_user_by_id` returns the fixed user (any other id
/// yields `None`); every other method delegates to the panic fake by being
/// unused.
struct OkUsers;

#[async_trait]
impl UserManager for OkUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(|| hsp_user_entity(USER_ID, "alice")))
    }
    // Remaining methods are never reached by these tests.
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!()
    }
    async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn change_password(
        &self,
        _user_id: Uuid,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn authenticate_user(
        &self,
        _username: &str,
        _password: &str,
        _remote_endpoint: &str,
        _is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_authentication_providers(
        &self,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_password_reset_providers(
        &self,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_dto(
        &self,
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<ferrofin_model::dto::UserDto, ServiceError> {
        Ok(ferrofin_model::dto::UserDto {
            id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
            name: Some(user.username.clone()),
            server_id,
            ..ferrofin_model::dto::UserDto::default()
        })
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &ferrofin_model::configuration::UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &ferrofin_model::users::UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// Builds a minimal [`BaseItemEntity`] with the given id + a fixed name; every
/// other field is `None`/`false`/empty ([`BaseItemEntity`] has no `Default`).
fn base_item_entity(id: Uuid) -> BaseItemEntity {
    BaseItemEntity {
        id: id.to_string(),
        album: None,
        album_artists: None,
        artists: None,
        audio: None,
        channel_id: None,
        clean_name: None,
        community_rating: None,
        critic_rating: None,
        custom_rating: None,
        data: None,
        date_created: None,
        date_last_media_added: None,
        date_last_refreshed: None,
        date_last_saved: None,
        date_modified: None,
        end_date: None,
        episode_title: None,
        external_id: None,
        external_series_id: None,
        external_service_id: None,
        extra_type: None,
        forced_sort_name: None,
        genres: None,
        height: None,
        index_number: None,
        inherited_parental_rating_sub_value: None,
        inherited_parental_rating_value: None,
        is_folder: false,
        is_in_mixed_folder: false,
        is_locked: false,
        is_movie: false,
        is_repeat: false,
        is_series: false,
        is_virtual_item: false,
        lufs: None,
        media_type: None,
        name: Some("Test Item".to_owned()),
        normalization_gain: None,
        official_rating: None,
        original_title: None,
        original_language: None,
        overview: None,
        owner_id: None,
        parent_id: None,
        parent_index_number: None,
        path: None,
        preferred_metadata_country_code: None,
        preferred_metadata_language: None,
        premiere_date: None,
        presentation_unique_key: None,
        primary_version_id: None,
        production_locations: None,
        production_year: None,
        run_time_ticks: None,
        season_id: None,
        season_name: None,
        series_id: None,
        series_name: None,
        series_presentation_unique_key: None,
        show_id: None,
        size: None,
        sort_name: None,
        start_date: None,
        studios: None,
        tagline: None,
        tags: None,
        top_parent_id: None,
        total_bitrate: None,
        type_: String::new(),
        unrated_type: None,
        width: None,
    }
}

/// A [`LibraryManager`] returning one item from `query_items`, and resolving a
/// single known item id in `get_item_by_id` (any other id is `None`).
struct OkLibrary {
    item_id: Uuid,
}

#[async_trait]
impl LibraryManager for OkLibrary {
    async fn is_item_visible_standalone(
        &self,
        _item: &ferrofin_db::entities::base_items::BaseItemEntity,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ferrofin_traits::error::ServiceError> {
        Ok(true)
    }
    async fn is_item_visible(
        &self,
        _item: &ferrofin_db::entities::base_items::BaseItemEntity,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ferrofin_traits::error::ServiceError> {
        Ok(true)
    }

    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == self.item_id).then(|| base_item_entity(self.item_id)))
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![base_item_entity(self.item_id)],
        ))
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_list(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn delete_item(
        &self,
        _id: Uuid,
        _options: &ferrofin_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_people_names(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        Ok(ferrofin_model::dto::ItemCounts {
            movie_count: 3,
            series_count: 1,
            ..ferrofin_model::dto::ItemCounts::default()
        })
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`UserViewManager`] returning one folder view.
struct OkUserViews {
    item_id: Uuid,
}

#[async_trait]
impl UserViewManager for OkUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![base_item_entity(self.item_id)])
    }
    async fn get_media_folders(&self) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(Uuid::nil()).await
    }
    async fn get_user_root_children(
        &self,
        _user: &UserEntity,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_media_folders().await
    }
    async fn get_latest_items(
        &self,
        _query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        unimplemented!()
    }
}

/// A [`DtoService`] projecting each entity into a `BaseItemDto` carrying the
/// entity's parsed id + name, so the JSON body shape can be asserted.
struct OkDto;

fn entity_to_dto(item: &BaseItemEntity) -> BaseItemDto {
    BaseItemDto {
        id: Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil()),
        name: item.name.clone(),
        ..BaseItemDto::default()
    }
}

#[async_trait]
impl DtoService for OkDto {
    async fn get_base_item_dto(
        &self,
        item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items.iter().map(entity_to_dto).collect())
    }
}

/// A [`MediaSourceManager`] returning one media source (with the given on-disk
/// path) from both the playback and static resolvers.
struct OkMediaSources {
    path: String,
}

fn media_source(path: &str) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some("source-1".to_owned()),
        path: Some(path.to_owned()),
        ..MediaSourceInfo::default()
    }
}

#[async_trait]
impl MediaSourceManager for OkMediaSources {
    async fn get_media_streams(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaStream>, ServiceError> {
        unimplemented!()
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaAttachment>, ServiceError> {
        unimplemented!()
    }
    async fn get_playback_media_sources(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        _enable_path_substitution: bool,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(vec![media_source(&self.path)])
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(vec![media_source(&self.path)])
    }
    async fn open_live_stream(
        &self,
        _request: &ferrofin_model::media_info::LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`SessionManager`] whose `authenticate_new_session` returns a canned
/// session for [`USER_ID`]; every other method is unused.
///
/// It records each request's `remote_endpoint`, which is what C#
/// `UserController.AuthenticateUserByName` fills from
/// `HttpContext.GetNormalizedRemoteIP()` and what every activity-log
/// `ShortOverview` ("IP address: …") is later built from.
#[derive(Default)]
struct OkSessions {
    seen_remote: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl SessionManager for OkSessions {
    async fn authenticate_new_session(
        &self,
        request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        self.seen_remote
            .lock()
            .expect("lock")
            .push(request.remote_endpoint.clone());
        Ok(AuthenticationResultData {
            session: SessionInfoDto {
                id: Some("session-1".to_owned()),
                user_id: USER_ID,
                user_name: Some("alice".to_owned()),
                server_id: Some("server-1".to_owned()),
                ..SessionInfoDto::default()
            },
            access_token: "canned-token".into(),
        })
    }
    async fn log_session_activity(
        &self,
        _app_name: &str,
        _app_version: &str,
        _device_id: &str,
        _device_name: &str,
        _remote_endpoint: &str,
        _user: Option<&UserEntity>,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!()
    }
    async fn update_device_name(
        &self,
        _session_id: &str,
        _reported_device_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn on_playback_start(
        &self,
        _info: &ferrofin_model::session::PlaybackStartInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn on_playback_progress(
        &self,
        _info: &ferrofin_model::session::PlaybackProgressInfo,
        _is_automated: bool,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn on_playback_stopped(
        &self,
        _info: &ferrofin_model::session::PlaybackStopInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn report_session_ended(&self, _session_id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_general_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &ferrofin_model::session::GeneralCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &ferrofin_model::session::MessageCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_play_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &ferrofin_model::session::PlayRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_playstate_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &ferrofin_model::session::PlaystateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_admin_sessions(
        &self,
        _message_type: ferrofin_model::session::SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_user_sessions(
        &self,
        _user_ids: &[Uuid],
        _message_type: ferrofin_model::session::SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        // The delete handler pushes UserDeleted here best-effort.
        Ok(())
    }
    async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn add_additional_user(
        &self,
        _session_id: &str,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn remove_additional_user(
        &self,
        _session_id: &str,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn report_now_viewing_item(
        &self,
        _session_id: &str,
        _item_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn authenticate_direct(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        unimplemented!()
    }
    async fn report_capabilities(
        &self,
        _session_id: &str,
        _capabilities: &ferrofin_model::session::ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn report_transcoding_info(
        &self,
        _device_id: &str,
        _info: &ferrofin_model::session::TranscodingInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_transcoding_info(&self, _device_id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_sessions(
        &self,
        _user_id: Uuid,
        _device_id: Option<&str>,
        _active_within_seconds: Option<i32>,
        _controllable_user_to_check: Option<Uuid>,
        _is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError> {
        unimplemented!()
    }
    async fn get_session_by_authentication_token(
        &self,
        _token: &str,
        _device_id: &str,
        _remote_endpoint: &str,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!()
    }
    async fn logout(&self, _access_token: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn logout_device(
        &self,
        _device: &ferrofin_db::entities::security::DeviceEntity,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn revoke_user_tokens(
        &self,
        _user_id: Uuid,
        _current_access_token: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn close_live_stream_if_needed(
        &self,
        _live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// Assembles an [`AppState`] wired for the success paths. `library`/`views`/
/// `media` share one item id and one media path; auth always succeeds.
fn ok_state(item_id: Uuid, media_path: &str) -> AppState {
    ok_state_recording(item_id, media_path).0
}

/// [`ok_state`] plus the handle its [`OkSessions`] records authenticate
/// requests' remote endpoints into.
fn ok_state_recording(
    item_id: Uuid,
    media_path: &str,
) -> (AppState, Arc<Mutex<Vec<Option<String>>>>) {
    let sessions = Arc::new(OkSessions::default());
    let seen_remote = Arc::clone(&sessions.seen_remote);
    let state = AppState::new(
        Arc::new(OkLibrary { item_id }),
        Arc::new(OkUsers),
        Arc::new(OkUserViews { item_id }),
        Arc::new(FakeUserData),
        Arc::new(OkMediaSources {
            path: media_path.to_owned(),
        }),
        sessions,
        Arc::new(FakeSystem),
        // FakeAppHost is fine — handlers under test never call it.
        Arc::new(ferrofin_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(ferrofin_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(ok_auth_context()),
        Arc::new(ok_auth_service()),
        Arc::new(ferrofin_api::test_support::FakeQuickConnect),
        Arc::new(ferrofin_api::test_support::FakePlaylists),
        Arc::new(ferrofin_api::test_support::FakeCollections),
        Arc::new(ferrofin_api::test_support::FakeTvSeries),
        Arc::new(ferrofin_api::test_support::FakeSubtitles),
        Arc::new(ferrofin_api::test_support::FakeLyrics),
        Arc::new(ferrofin_api::test_support::FakeMediaSegments),
        Arc::new(ferrofin_api::test_support::FakeTrickplay),
        Arc::new(ferrofin_api::test_support::FakeDevices),
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    );
    (state, seen_remote)
}

/// Reads a response body into a JSON value.
async fn json_body(response: axum::response::Response) -> serde_json::Value {
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
async fn delete_user_writes_a_user_deleted_activity_entry() {
    // Port of upstream's UserDeletedLogger: the dashboard activity feed gets
    // "User bob has been deleted" tagged with `Guid.Empty` — NOT the deleted
    // user's id. Live Jellyfin 10.11.8 puts
    // `UserId: "00000000000000000000000000000000"` on this entry; tagging it
    // with the real id also leaked it into the `hasUserId=true` filter.
    let activity = Arc::new(ferrofin_api::test_support::RecordingActivity::default());
    let router = create_router(state_with_activity(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
        activity.clone(),
    ));
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
    let entries = activity.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].type_, "UserDeleted");
    assert_eq!(entries[0].name, "User bob has been deleted");
    assert_eq!(
        entries[0].user_id, None,
        "UserDeletedLogger passes Guid.Empty"
    );
}

/// An [`ActivityManager`](ferrofin_traits::activity::ActivityManager) whose
/// every write fails, as a full disk or a locked database makes it.
struct FailingActivity;

#[async_trait]
impl ferrofin_traits::activity::ActivityManager for FailingActivity {
    async fn get_paged_result(
        &self,
        _query: &ferrofin_traits::activity::ActivityLogQuery,
    ) -> Result<QueryResult<ferrofin_model::activity::ActivityLogEntry>, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_entry(
        &self,
        _entry: ferrofin_traits::activity::ActivityLogCreate,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::backend("database is locked"))
    }
    async fn clean(&self, _before: chrono::DateTime<chrono::Utc>) -> Result<u64, ServiceError> {
        Ok(0)
    }
}

/// A failed activity write does not fail the request, but it is logged with
/// the entry's type: the audit row is lost, and the log is the only trace.
#[tokio::test]
async fn a_failed_activity_write_is_logged_not_returned() {
    let router = create_router(state_with_activity(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
        Arc::new(FailingActivity),
    ));
    let log = CapturedLog::default();
    let writer = log.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
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
    let logged = log.take();
    assert!(
        logged.contains("WARN")
            && logged.contains("UserDeleted")
            && logged.contains("database is locked"),
        "{logged}"
    );
}

/// A [`PlaylistManager`](ferrofin_traits::collections::PlaylistManager) that
/// records each `remove_playlists` call with whether the user was already
/// deleted then; every other method is unused here.
struct RecordingPlaylists {
    users: Arc<MemUsers>,
    removed: Mutex<Vec<(Uuid, bool)>>,
    fail: bool,
}

impl RecordingPlaylists {
    fn new(users: Arc<MemUsers>, fail: bool) -> Self {
        Self {
            users,
            removed: Mutex::new(Vec::new()),
            fail,
        }
    }
}

#[async_trait]
impl ferrofin_traits::collections::PlaylistManager for RecordingPlaylists {
    async fn get_playlist_access(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<ferrofin_traits::collections::PlaylistAccess, ServiceError> {
        unimplemented!("unused")
    }
    async fn create_playlist(
        &self,
        _request: &ferrofin_model::playlists::PlaylistCreationRequest,
    ) -> Result<ferrofin_model::playlists::PlaylistCreationResult, ServiceError> {
        unimplemented!("unused")
    }
    async fn update_playlist(
        &self,
        _request: &ferrofin_model::playlists::PlaylistUpdateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_playlist_items(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<ferrofin_traits::persistence::PlaylistMembers, ServiceError> {
        unimplemented!("unused")
    }
    async fn get_playlist_item_ids(&self, _playlist_id: Uuid) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("unused")
    }
    async fn add_user_to_shares(
        &self,
        _request: &ferrofin_model::playlists::PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn remove_user_from_shares(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
        _share: &ferrofin_model::entities_media::PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn get_playlist_shares(
        &self,
        _playlist_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::PlaylistUserPermissions>, ServiceError> {
        unimplemented!("unused")
    }
    async fn add_item_to_playlist(
        &self,
        _playlist_id: Uuid,
        _item_ids: &[Uuid],
        _position: Option<i32>,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn remove_item_from_playlist(
        &self,
        _playlist_id: &str,
        _entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn move_item(
        &self,
        _playlist_id: &str,
        _entry_id: &str,
        _new_index: i32,
        _calling_user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    async fn remove_playlists(&self, user_id: Uuid) -> Result<(), ServiceError> {
        let deleted = self.users.deleted.lock().unwrap().is_some();
        self.removed.lock().unwrap().push((user_id, deleted));
        if self.fail {
            return Err(ServiceError::backend("playlist store unavailable"));
        }
        Ok(())
    }
}

/// The admin's state with `playlists` as the playlist manager.
fn recording_state(
    users: Arc<MemUsers>,
    activity: Arc<dyn ferrofin_traits::activity::ActivityManager>,
    playlists: Arc<RecordingPlaylists>,
) -> AppState {
    state_with_playlists(
        users,
        Arc::new(MemConfig::new(true)),
        activity,
        Arc::new(admin_auth()),
        Arc::new(NoopSessions::default()),
        playlists,
    )
}

async fn delete_bob(state: AppState) -> StatusCode {
    create_router(state)
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Users/{BOB_ID}"))
                .header("X-Emby-Token", "t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// `UserController.DeleteUser` → `RemovePlaylistsAsync(userId)`: the deleted
/// user's playlists are handed over or deleted, once the delete has succeeded.
#[tokio::test]
async fn delete_user_hands_over_the_users_playlists_after_the_delete() {
    let users = Arc::new(MemUsers::default());
    let playlists = Arc::new(RecordingPlaylists::new(users.clone(), false));
    let state = recording_state(
        users,
        Arc::new(ferrofin_api::test_support::FakeActivity),
        playlists.clone(),
    );
    assert_eq!(delete_bob(state).await, StatusCode::NO_CONTENT);
    assert_eq!(*playlists.removed.lock().unwrap(), vec![(BOB_ID, true)]);
}

/// A refused delete (the last user or the last admin) leaves the user's
/// playlists alone; upstream has already given them away by then.
#[tokio::test]
async fn a_refused_delete_leaves_the_users_playlists_alone() {
    let users = Arc::new(MemUsers::default());
    users
        .refuse_delete
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let playlists = Arc::new(RecordingPlaylists::new(users.clone(), false));
    let state = recording_state(
        users,
        Arc::new(ferrofin_api::test_support::FakeActivity),
        playlists.clone(),
    );
    assert_eq!(delete_bob(state).await, StatusCode::BAD_REQUEST);
    assert!(playlists.removed.lock().unwrap().is_empty());
}

/// A failed hand-over fails the request, but the deletion it follows still
/// gets its `UserDeleted` activity entry.
#[tokio::test]
async fn a_failed_playlist_hand_over_still_logs_the_deletion() {
    let users = Arc::new(MemUsers::default());
    let activity = Arc::new(ferrofin_api::test_support::RecordingActivity::default());
    let playlists = Arc::new(RecordingPlaylists::new(users.clone(), true));
    let state = recording_state(users.clone(), activity.clone(), playlists);
    assert!(delete_bob(state).await.is_server_error());
    assert_eq!(*users.deleted.lock().unwrap(), Some(BOB_ID));
    let entries = activity.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].type_, "UserDeleted");
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
                .body(Body::from(serde_json::to_vec(&complete_policy()).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    // Updating bob's policy is fine and is recorded.
    let policy = UserPolicy {
        max_active_sessions: 5,
        ..complete_policy()
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

/// Upstream binds the body into `new UserPolicy()`, so a member it leaves out
/// keeps the constructor's value (`UserPolicy.cs`), not `false`/`0`.
#[tokio::test]
async fn update_policy_partial_body_keeps_the_constructor_defaults() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let body = serde_json::json!({
        "AuthenticationProviderId": "a",
        "PasswordResetProviderId": "b",
        "IsDisabled": false,
    });
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{BOB_ID}/Policy"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (_, saved) = users.updated_policy.lock().unwrap().clone().unwrap();
    assert_eq!(
        saved,
        UserPolicy {
            is_hidden: true,
            enable_user_preference_access: true,
            enable_shared_device_control: true,
            enable_remote_access: true,
            enable_live_tv_management: true,
            enable_live_tv_access: true,
            enable_media_playback: true,
            enable_audio_playback_transcoding: true,
            enable_video_playback_transcoding: true,
            enable_playback_remuxing: true,
            enable_content_downloading: true,
            enable_sync_transcoding: true,
            enable_media_conversion: true,
            enable_all_devices: true,
            enable_all_channels: true,
            enable_all_folders: true,
            login_attempts_before_lockout: -1,
            enable_public_sharing: true,
            authentication_provider_id: "a".to_owned(),
            password_reset_provider_id: "b".to_owned(),
            ..UserPolicy::default()
        }
    );
}

/// `UserManager.GetUserById(Guid.Empty)` throws `ArgumentException`, a `400`:
/// a nil route id, a nil `?userId=`, or an API key's empty user claim when
/// `userId` is left out (`userId ?? User.GetUserId()`).
#[rstest::rstest]
#[case::get("GET", "/Users/00000000-0000-0000-0000-000000000000", "", false)]
#[case::delete("DELETE", "/Users/00000000-0000-0000-0000-000000000000", "", false)]
#[case::policy(
    "POST",
    "/Users/00000000-0000-0000-0000-000000000000/Policy",
    r#"{"AuthenticationProviderId":"a","PasswordResetProviderId":"b"}"#,
    false
)]
#[case::update(
    "POST",
    "/Users?userId=00000000-0000-0000-0000-000000000000",
    r#"{"Name":"bob","Policy":null}"#,
    false
)]
#[case::update_path(
    "POST",
    "/Users/00000000-0000-0000-0000-000000000000",
    r#"{"Name":"bob","Policy":null}"#,
    false
)]
#[case::configuration(
    "POST",
    "/Users/Configuration?userId=00000000-0000-0000-0000-000000000000",
    "{}",
    false
)]
#[case::configuration_path(
    "POST",
    "/Users/00000000-0000-0000-0000-000000000000/Configuration",
    "{}",
    false
)]
#[case::password_path(
    "POST",
    "/Users/00000000-0000-0000-0000-000000000000/Password",
    r#"{"ResetPassword":true}"#,
    false
)]
#[case::password(
    "POST",
    "/Users/Password?userId=00000000-0000-0000-0000-000000000000",
    r#"{"ResetPassword":true}"#,
    false
)]
#[case::api_key_update("POST", "/Users", r#"{"Name":"bob","Policy":null}"#, true)]
#[case::api_key_configuration("POST", "/Users/Configuration", "{}", true)]
#[case::api_key_password("POST", "/Users/Password", r#"{"ResetPassword":true}"#, true)]
#[tokio::test]
async fn a_nil_user_id_is_a_bad_request(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] body: &str,
    #[case] api_key: bool,
) {
    let users = Arc::new(MemUsers::default());
    let auth: Arc<dyn AuthService> = if api_key {
        Arc::new(ferrofin_api::test_support::ApiKeyAuthService)
    } else {
        Arc::new(admin_auth())
    };
    let router = create_router(state_with_auth(
        users.clone(),
        Arc::new(MemConfig::new(true)),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        auth,
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(users.updated_policy.lock().unwrap().is_none());
    assert!(users.deleted.lock().unwrap().is_none());
    assert!(users.updated_configuration.lock().unwrap().is_none());
}

/// A policy whose two `[Required]` provider ids are set, as every client that
/// echoes a `GET /Users/{id}` policy back sends it.
fn complete_policy() -> UserPolicy {
    UserPolicy {
        authentication_provider_id:
            "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider".to_owned(),
        password_reset_provider_id:
            "Jellyfin.Server.Implementations.Users.DefaultPasswordResetProvider".to_owned(),
        ..UserPolicy::default()
    }
}

/// `UserPolicy`'s provider ids are `[Required(AllowEmptyStrings = false)]`, so
/// ASP.NET's model validation refuses a policy body that leaves either out,
/// sends it null or blank, before the action runs (ahead of the unknown user's
/// 404), naming each failing member. Measured on a live Jellyfin 12.2.
#[rstest::rstest]
#[case::empty_body(r"{}", BOB_ID, &["AuthenticationProviderId", "PasswordResetProviderId"])]
#[case::no_reset_provider(r#"{"AuthenticationProviderId":"a"}"#, BOB_ID, &["PasswordResetProviderId"])]
#[case::empty_auth_provider(
    r#"{"AuthenticationProviderId":"","PasswordResetProviderId":"b"}"#,
    BOB_ID,
    &["AuthenticationProviderId"]
)]
#[case::blank_auth_provider(
    r#"{"AuthenticationProviderId":"  ","PasswordResetProviderId":"b"}"#,
    BOB_ID,
    &["AuthenticationProviderId"]
)]
#[case::null_reset_provider(
    r#"{"AuthenticationProviderId":"a","PasswordResetProviderId":null}"#,
    BOB_ID,
    &["PasswordResetProviderId"]
)]
#[case::unknown_user(r"{}", Uuid::from_u128(0xDEAD), &["AuthenticationProviderId", "PasswordResetProviderId"])]
#[case::nil_user(r"{}", Uuid::nil(), &["AuthenticationProviderId", "PasswordResetProviderId"])]
#[tokio::test]
async fn update_policy_requires_both_provider_ids(
    #[case] body: &str,
    #[case] user: Uuid,
    #[case] failing: &[&str],
) {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{user}/Policy"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response).await;
    let errors = json["errors"].as_object().expect("validation errors");
    let mut keys: Vec<&str> = errors.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, failing);
    for member in failing {
        assert_eq!(
            errors[*member][0],
            format!("The {member} field is required.")
        );
    }
    assert!(
        users.updated_policy.lock().unwrap().is_none(),
        "nothing saved"
    );
}

// The flow itself (PIN files, local-only issue, one reset per PIN, expiry)
// lives in the user manager and is tested there, in ferrofin-core
// (`user_manager/password_reset_tests.rs`). These check the handlers' wiring.

#[tokio::test]
async fn forgot_password_hands_the_name_and_network_to_the_user_manager() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    for peer in ["203.0.113.8:4321", "127.0.0.1:4321"] {
        let response = recovery_request(
            &router,
            "/Users/ForgotPassword",
            serde_json::json!({ "EnteredUsername": "bob" }),
            peer,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["Action"], "PinCode");
        assert_eq!(body["PinFile"], "/data/passwordreset0123.json");
        assert!(body["PinExpirationDate"].is_string());
    }
    assert_eq!(
        *users.forgot_requests.lock().unwrap(),
        vec![("bob".to_owned(), false), ("bob".to_owned(), true)]
    );
}

async fn recovery_request(
    router: &axum::Router,
    path: &str,
    body: serde_json::Value,
    peer: &str,
) -> axum::response::Response {
    raw_recovery_request(router, path, &body.to_string(), peer, None).await
}

/// [`recovery_request`] with the body as sent, and the address the
/// connection was accepted on when `local` names one.
async fn raw_recovery_request(
    router: &axum::Router,
    path: &str,
    body: &str,
    peer: &str,
    local: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .extension(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
    if let Some(local) = local {
        request = request.extension(ferrofin_api::ip_access::ConnectionLocalAddr(
            local.parse().unwrap(),
        ));
    }
    router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

/// Upstream's `EnteredUsername` and `Pin` are `[Required] public required
/// string`. System.Text.Json refuses a body that leaves the member out (keyed
/// `$`), and `[Required]` then refuses a `null`, empty or whitespace one,
/// keyed by the member alone. Both refusals are 400s before the action runs.
/// The messages were measured on Jellyfin 12.4.0.
#[rstest::rstest]
#[case::name_left_out(
    "/Users/ForgotPassword",
    "{}",
    "$",
    "JSON deserialization for type 'Jellyfin.Api.Models.UserDtos.ForgotPasswordDto' was \
     missing required properties including: 'EnteredUsername'."
)]
#[case::name_null(
    "/Users/ForgotPassword",
    r#"{"EnteredUsername":null}"#,
    "EnteredUsername",
    "The EnteredUsername field is required."
)]
#[case::name_empty(
    "/Users/ForgotPassword",
    r#"{"EnteredUsername":""}"#,
    "EnteredUsername",
    "The EnteredUsername field is required."
)]
#[case::name_whitespace(
    "/Users/ForgotPassword",
    r#"{"enteredUsername":" \t　"}"#,
    "EnteredUsername",
    "The EnteredUsername field is required."
)]
#[case::pin_left_out(
    "/Users/ForgotPassword/Pin",
    r#"{"EnteredUsername":"bob"}"#,
    "$",
    "JSON deserialization for type 'Jellyfin.Api.Models.UserDtos.ForgotPasswordPinDto' was \
     missing required properties including: 'Pin'."
)]
#[case::pin_null(
    "/Users/ForgotPassword/Pin",
    r#"{"Pin":null}"#,
    "Pin",
    "The Pin field is required."
)]
#[case::pin_empty(
    "/Users/ForgotPassword/Pin",
    r#"{"Pin":""}"#,
    "Pin",
    "The Pin field is required."
)]
#[case::pin_whitespace(
    "/Users/ForgotPassword/Pin",
    r#"{"pin":"  "}"#,
    "Pin",
    "The Pin field is required."
)]
#[tokio::test]
async fn forgot_password_routes_refuse_a_missing_or_blank_member_with_400(
    #[case] path: &str,
    #[case] body: &str,
    #[case] key: &str,
    #[case] message: &str,
) {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let response = raw_recovery_request(&router, path, body, "127.0.0.1:4321", None).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["title"], "One or more validation errors occurred.");
    assert_eq!(body["errors"][key], serde_json::json!([message]), "{body}");
    if key != "$" {
        assert_eq!(
            body["errors"].as_object().unwrap().len(),
            1,
            "a member refusal names the member alone: {body}"
        );
    }
    assert!(users.forgot_requests.lock().unwrap().is_empty());
    assert!(users.redeemed_pins.lock().unwrap().is_empty());
}

/// Everything logged while a test runs.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    /// Takes what was logged since the last call.
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

/// `UserController.ForgotPassword` warns with the client's address when the
/// request does not come from the local network, where `isLocal` is
/// `HttpContext.IsLocal()` (the connection's local address is the client's) or
/// `IsInLocalNetwork(ip)`. That flag is also what the user manager is given.
#[tokio::test]
async fn forgot_password_warns_with_the_address_of_a_request_from_outside_the_local_network() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let log = CapturedLog::default();
    let writer = log.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let body = r#"{"EnteredUsername":"bob"}"#;
    let path = "/Users/ForgotPassword";

    let remote = raw_recovery_request(&router, path, body, "203.0.113.8:4321", None).await;
    assert_eq!(remote.status(), StatusCode::OK);
    let logged = log.take();
    assert!(
        logged.contains("WARN")
            && logged.contains("outside the local network")
            && logged.contains("remote_ip=203.0.113.8"),
        "{logged}"
    );

    for (peer, local) in [
        ("127.0.0.1:4321", None),
        ("192.168.1.20:4321", None),
        ("203.0.113.8:4321", Some("203.0.113.8:8096")),
    ] {
        let response = raw_recovery_request(&router, path, body, peer, local).await;
        assert_eq!(response.status(), StatusCode::OK);
        let logged = log.take();
        assert!(
            !logged.contains("outside the local network"),
            "{peer} {local:?}: {logged}"
        );
    }
    // A refused body never reaches the action, so it logs nothing.
    let refused = raw_recovery_request(&router, path, "{}", "203.0.113.8:4321", None).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(!log.take().contains("outside the local network"));

    assert_eq!(
        *users.forgot_requests.lock().unwrap(),
        vec![
            ("bob".to_owned(), false),
            ("bob".to_owned(), true),
            ("bob".to_owned(), true),
            ("bob".to_owned(), true),
        ]
    );
}

#[tokio::test]
async fn forgot_password_pin_answers_the_redemption_and_404s_a_pin_no_provider_redeems() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let redeem = "/Users/ForgotPassword/Pin";
    let local = "127.0.0.1:4321";
    let response = recovery_request(
        &router,
        redeem,
        serde_json::json!({"Pin":"A1B2C3D4"}),
        local,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["Success"], true);
    assert_eq!(body["UsersReset"], serde_json::json!(["bob"]));
    let wrong = recovery_request(
        &router,
        redeem,
        serde_json::json!({"Pin":"a1-b2-c3-d4"}),
        local,
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        *users.redeemed_pins.lock().unwrap(),
        vec!["A1B2C3D4".to_owned(), "a1-b2-c3-d4".to_owned()],
        "the PIN reaches the user manager exactly as entered"
    );
}

#[tokio::test]
async fn authenticate_by_name_returns_authentication_result() {
    let router = create_router(ok_state(Uuid::from_u128(1), ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Username":"alice","Pw":"secret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    // AuthenticationResult carries the session's user + session info.
    assert_eq!(json["SessionInfo"]["Id"], "session-1");
    assert_eq!(json["User"]["Id"], USER_ID.simple().to_string());
    assert_eq!(json["User"]["Name"], "alice");
    assert_eq!(json["ServerId"], "server-1");
    // ...and the minted access token the client must present on later requests.
    assert_eq!(json["AccessToken"], "canned-token");
}

/// A sign-in whose user cannot be reloaded afterwards (here: gone from the
/// store) still answers with the minted token, and its bare `User` carries
/// the session's `ServerId` and `new UserDto()`'s policy, `new UserPolicy()`,
/// not an all-false one that tells the client the user may not play anything.
#[tokio::test]
async fn sign_in_fallback_user_carries_the_constructor_policy() {
    let router = create_router(state_with_sessions(
        Arc::new(MemUsers::default()),
        Arc::new(MemConfig::new(true)),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(admin_auth()),
        Arc::new(OkSessions::default()),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Username":"alice","Pw":"secret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["AccessToken"], "canned-token");
    assert_eq!(json["User"]["Name"], "alice");
    assert_eq!(json["User"]["ServerId"], "server-1");
    let policy = &json["User"]["Policy"];
    // `UserPolicy.cs`'s constructor values.
    assert_eq!(policy["EnableMediaPlayback"], true);
    assert_eq!(policy["EnableAllFolders"], true);
    assert_eq!(policy["IsHidden"], true);
    assert_eq!(policy["IsAdministrator"], false);
    assert_eq!(policy["LoginAttemptsBeforeLockout"], -1);
    assert_eq!(policy["SyncPlayAccess"], "CreateAndJoinGroups");
    // Every member: an empty body binds the constructor policy.
    let constructor: UserPolicy = serde_json::from_str("{}").unwrap();
    assert_eq!(*policy, serde_json::to_value(constructor).unwrap());
}

/// The obsolete `UserController.AuthenticateUser` (`POST /Users/{userId}/
/// Authenticate?pw=`, hidden from the OpenAPI document but served): anonymous,
/// 400 for a nil id, 404 for an unknown user, `[Required] pw`, else
/// `AuthenticateUserByName`.
#[tokio::test]
async fn authenticate_by_id_signs_in_as_that_user() {
    let post = |uri: String| async move {
        create_router(ok_state(Uuid::from_u128(1), ""))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    };
    let response = post(format!("/Users/{USER_ID}/Authenticate?pw=secret")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["User"]["Name"], "alice");
    assert_eq!(json["AccessToken"], "canned-token");
    let response = post(format!(
        "/Users/{}/Authenticate?pw=secret",
        Uuid::from_u128(0xdead)
    ))
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = post(format!("/Users/{}/Authenticate?pw=secret", Uuid::nil())).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "GetUserById(Guid.Empty) throws"
    );
    let response = post(format!("/Users/{USER_ID}/Authenticate")).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "pw is [Required]"
    );
}

/// C# `UserController.AuthenticateUserByName` sets
/// `RemoteEndPoint = HttpContext.GetNormalizedRemoteIP().ToString()` on the
/// authentication request. Ferrofin passed `None`, so `SessionInfo.RemoteEndPoint`
/// was blank on every session and every activity-log `ShortOverview` derived from
/// it ("IP address: …") came out null. With no `ConnectInfo` on the request the
/// resolver falls back to loopback, exactly as `GetNormalizedRemoteIP` defaults it.
#[tokio::test]
async fn authenticate_by_name_carries_the_callers_address() {
    let (state, seen_remote) = ok_state_recording(Uuid::from_u128(1), "");
    let router = create_router(state);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Username":"alice","Pw":"secret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *seen_remote.lock().expect("lock"),
        vec![Some("127.0.0.1".to_owned())]
    );
}

#[tokio::test]
async fn current_user_returns_user_dto() {
    let router = create_router(ok_state(Uuid::from_u128(1), ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Users/Me")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["Id"], USER_ID.simple().to_string());
    assert_eq!(json["Name"], "alice");
    assert_eq!(json["HasPassword"], true);
}

// The path-scoped `/Users/{userId}` update/password/configuration aliases
// (upstream keeps them [Obsolete] + hidden from OpenAPI, jellyfin-web's
// apiclient still calls them) must forward to the query-scoped handlers —
// executing the real bodies, not just the 401 registration check.

#[tokio::test]
async fn user_scoped_update_user_forwards() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    // Same name → no rename; the configuration write is the observable effect.
    let body = serde_json::json!({ "Name": "bob", "Configuration": {}, "Policy": null });
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{BOB_ID}"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // The path user id reaches the manager (proving the alias forwarded).
    let (id, _) = users.updated_configuration.lock().unwrap().clone().unwrap();
    assert_eq!(id, BOB_ID);
}

/// Posts `body` to `POST /Users?userId={bob}` as the administrator, returning
/// the status, the JSON body (`Null` when empty) and the store it wrote to.
async fn update_bob(body: &str) -> (StatusCode, serde_json::Value, Arc<MemUsers>) {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users?userId={BOB_ID}"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, users)
}

/// ASP.NET validates the `Policy` nested in a `UserDto` body before the
/// action runs. A body without one carries `UserDto`'s constructor policy,
/// whose `[Required]` provider ids are null, so it is a 400 keyed
/// `Policy.{member}`; only a complete policy or `"Policy": null` passes.
/// Measured on a live Jellyfin 12.2.
#[rstest::rstest]
#[case::no_policy(
    r#"{"Name":"bob"}"#,
    &["Policy.AuthenticationProviderId", "Policy.PasswordResetProviderId"]
)]
#[case::empty_policy(
    r#"{"Name":"bob","Policy":{}}"#,
    &["Policy.AuthenticationProviderId", "Policy.PasswordResetProviderId"]
)]
#[case::blank_reset_provider(
    r#"{"Name":"bob","Policy":{"AuthenticationProviderId":"a","PasswordResetProviderId":" "}}"#,
    &["Policy.PasswordResetProviderId"]
)]
#[case::null_auth_provider(
    r#"{"Name":"bob","Policy":{"AuthenticationProviderId":null,"PasswordResetProviderId":"b"}}"#,
    &["Policy.AuthenticationProviderId"]
)]
#[tokio::test]
async fn update_user_validates_the_nested_policy(#[case] body: &str, #[case] failing: &[&str]) {
    let (status, json, users) = update_bob(body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let errors = json["errors"].as_object().expect("validation errors");
    let mut keys: Vec<&str> = errors.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, failing);
    assert!(users.updated_configuration.lock().unwrap().is_none());
}

/// The hidden `POST /Users/{userId}` forwards to the same validation.
#[tokio::test]
async fn user_scoped_update_user_validates_the_nested_policy() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{BOB_ID}"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"Name":"bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response).await;
    assert!(json["errors"]["Policy.AuthenticationProviderId"].is_array());
    assert!(users.updated_configuration.lock().unwrap().is_none());
}

#[rstest::rstest]
#[case::null_policy(r#"{"Name":"bob","Policy":null}"#)]
#[case::complete_policy(
    r#"{"Name":"bob","Policy":{"AuthenticationProviderId":"a","PasswordResetProviderId":"b"}}"#
)]
#[tokio::test]
async fn update_user_accepts_a_complete_or_null_policy(#[case] body: &str) {
    let (status, _, _) = update_bob(body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// C# compares the stored name with `updateUser.Name` and renames when they
/// differ, so a missing or null name is handed to `RenameUser`, whose
/// `ThrowIfInvalidUsername` refuses it (the real manager's
/// `require_valid_username`, a 400). The same name renames nothing.
#[rstest::rstest]
#[case::no_name(r#"{"Policy":null}"#, Some(""))]
#[case::null_name(r#"{"Name":null,"Policy":null}"#, Some(""))]
#[case::same_name(r#"{"Name":"bob","Policy":null}"#, None)]
#[tokio::test]
async fn update_user_renames_whenever_the_name_differs(
    #[case] body: &str,
    #[case] renamed_to: Option<&str>,
) {
    let (status, _, users) = update_bob(body).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the fake store renames to anything"
    );
    let renamed = users.renamed.lock().unwrap().clone();
    assert_eq!(renamed.as_ref().map(|(_, name)| name.as_str()), renamed_to);
}

/// `UserDto`'s constructor supplies a default `UserConfiguration`, so a body
/// without `Configuration` resets the stored settings to the defaults, as a
/// live Jellyfin 12.2 does. `"Configuration": null` (a 500 upstream) saves
/// nothing.
#[tokio::test]
async fn update_user_without_configuration_saves_the_default_configuration() {
    let (status, _, users) = update_bob(r#"{"Name":"bob","Policy":null}"#).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let saved = users.updated_configuration.lock().unwrap().clone();
    assert_eq!(saved, Some((BOB_ID, UserConfiguration::default())));

    let (status, _, users) =
        update_bob(r#"{"Name":"bob","Policy":null,"Configuration":null}"#).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(users.updated_configuration.lock().unwrap().is_none());
}

#[tokio::test]
async fn user_scoped_password_change_forwards() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    // Admin caller changing bob's password: no current-pw proof required.
    let body = serde_json::json!({ "NewPw": "s3cret" });
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{BOB_ID}/Password"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (id, pw) = users.changed_password.lock().unwrap().clone().unwrap();
    assert_eq!(id, BOB_ID);
    assert_eq!(pw, "s3cret");
}

#[tokio::test]
async fn user_scoped_configuration_forwards() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));
    let body = serde_json::json!({ "AudioLanguagePreference": "fr" });
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users/{BOB_ID}/Configuration"))
                .header("X-Emby-Token", "t")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // The path user id and the parsed configuration reach the manager.
    let (id, config) = users.updated_configuration.lock().unwrap().clone().unwrap();
    assert_eq!(id, BOB_ID);
    assert_eq!(config.audio_language_preference.as_deref(), Some("fr"));
}

/// `assert_can_update_user` compares the caller's id with the target's stored
/// id. `AuthorizationInfo::user_id()` is the nil GUID for a caller with no user
/// (an API key), so degrading an unparseable *target* id to nil as well made the
/// two compare equal and granted the update — C# compares against `User.Id`, a
/// `Guid` that is never `Guid.Empty` for a loaded user. Even an elevated API
/// key must not turn an invalid stored id into an anonymous self-update.
#[tokio::test]
async fn a_userless_caller_cannot_update_a_user_whose_stored_id_is_malformed() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state_with_auth(
        users,
        Arc::new(MemConfig::new(true)),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Users?userId={CORRUPT_ID}"))
                .header("X-Emby-Token", "t")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Name":"pwned","Policy":null}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a caller with no user must not be able to update a corrupt-id row"
    );
}

#[tokio::test]
async fn changing_password_preserves_the_requesting_session() {
    let users = Arc::new(MemUsers::default());
    let sessions = Arc::new(NoopSessions::default());
    let app = state_with_sessions(
        users,
        Arc::new(MemConfig::new(true)),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(admin_auth()),
        sessions.clone(),
    );
    let router = create_router(app);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/Password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"NewPw":"replacement"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        *sessions.revoked.lock().unwrap(),
        vec![(ADMIN_ID, "current-session".to_owned())]
    );
}

/// A normal account editing itself with preferences disabled must be denied,
/// while an API key retains its upstream administrator role.
#[tokio::test]
async fn account_updates_respect_preferences_and_api_key_elevation() {
    fn bob_auth() -> FixedAuth {
        FixedAuth::signed_in(user_entity(BOB_ID, "bob", None))
    }

    for (path, body) in [
        ("/Users", r#"{"Name":"new-name","Policy":null}"#),
        ("/Users/Password", r#"{"NewPw":"replacement"}"#),
        (
            "/Users/Configuration",
            r#"{"AudioLanguagePreference":"fr"}"#,
        ),
    ] {
        for (auth, expected) in [
            (
                Arc::new(bob_auth()) as Arc<dyn AuthService>,
                StatusCode::FORBIDDEN,
            ),
            (
                Arc::new(ferrofin_api::test_support::ApiKeyAuthService) as Arc<dyn AuthService>,
                StatusCode::NO_CONTENT,
            ),
        ] {
            let app = state_with_auth(
                Arc::new(MemUsers::default()),
                Arc::new(MemConfig::new(true)),
                Arc::new(ferrofin_api::test_support::FakeActivity),
                auth,
            );
            let response = create_router(app)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("{path}?userId={BOB_ID}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_owned()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
    }
}

/// Authenticates by the presented token, so one router serves every caller the
/// `Policy`/`Configuration` rule tells apart. No known token is anonymous.
struct TokenAuth;

#[async_trait]
impl AuthService for TokenAuth {
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        let token = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("X-Emby-Token"))
            .map(|(_, value)| value.as_str());
        let signed_in = |user: UserEntity| AuthorizationInfo {
            user: Some(Arc::new(user)),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        };
        Ok(match token {
            Some("admin") => signed_in(user_entity(ADMIN_ID, "admin", Some("hash"))),
            Some("bob") => signed_in(user_entity(BOB_ID, "bob", None)),
            Some("key") => AuthorizationInfo {
                is_authenticated: true,
                is_api_key: true,
                ..AuthorizationInfo::default()
            },
            _ => AuthorizationInfo::default(),
        })
    }
}

/// `GET path` as `token` (anonymous when `None`), from `peer`, on a wizard state.
async fn get_as(
    users: Arc<MemUsers>,
    wizard_done: bool,
    path: &str,
    token: Option<&str>,
    peer: &str,
) -> serde_json::Value {
    let app = state_with_auth(
        users,
        Arc::new(MemConfig::new(wizard_done)),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(TokenAuth),
    );
    let mut request = Request::builder()
        .uri(path)
        .extension(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
    if let Some(token) = token {
        request = request.header("X-Emby-Token", token);
    }
    let response = create_router(app)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    body_json(response).await
}

/// Asserts a row carries both private objects, or leaves both out of the JSON
/// (how Jellyfin writes a null, and how the contract's optional nullable
/// fields allow it), and that the login card's fields stay either way.
fn assert_private_fields(row: &serde_json::Value, whole: bool) {
    let name = &row["Name"];
    assert!(name.is_string(), "{row}");
    assert!(row["Id"].is_string(), "{row}");
    for field in ["Policy", "Configuration"] {
        match row.get(field) {
            Some(value) => assert!(whole && value.is_object(), "{name}: {field} is {value}"),
            None => assert!(!whole, "{name}: {field} is missing"),
        }
    }
}

/// Owner decision gl-1102, beyond Jellyfin: only an administrator or an API key
/// reads another account's `Policy` and `Configuration`. A user keeps their own.
#[rstest::rstest]
#[case::list_non_admin("/Users", Some("bob"), &["bob"])]
#[case::list_admin("/Users", Some("admin"), &["admin", "bob"])]
#[case::list_api_key("/Users", Some("key"), &["admin", "bob"])]
#[case::public_anonymous("/Users/Public", None, &[])]
#[case::public_unknown_token("/Users/Public", Some("stale"), &[])]
#[case::public_non_admin("/Users/Public", Some("bob"), &["bob"])]
#[case::public_admin("/Users/Public", Some("admin"), &["admin", "bob"])]
#[case::public_api_key("/Users/Public", Some("key"), &["admin", "bob"])]
#[tokio::test]
async fn user_lists_keep_private_fields_from_other_accounts(
    #[case] path: &str,
    #[case] token: Option<&str>,
    #[case] whole: &[&str],
    #[values(false, true)] wizard_done: bool,
) {
    let users = Arc::new(MemUsers::default());
    let body = get_as(users, wizard_done, path, token, "127.0.0.1:9000").await;
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "no row is dropped: {body}");
    for row in rows {
        assert_private_fields(row, whole.contains(&row["Name"].as_str().unwrap()));
    }
}

#[rstest::rstest]
#[case::another_account(ADMIN_ID, "bob", false)]
#[case::own_account(BOB_ID, "bob", true)]
#[case::admin_reads_another(BOB_ID, "admin", true)]
#[case::api_key_reads_any(ADMIN_ID, "key", true)]
#[tokio::test]
async fn get_user_by_id_keeps_private_fields_from_other_accounts(
    #[case] target: Uuid,
    #[case] token: &str,
    #[case] whole: bool,
) {
    let users = Arc::new(MemUsers::default());
    let path = format!("/Users/{target}");
    let row = get_as(users, true, &path, Some(token), "127.0.0.1:9000").await;
    assert_private_fields(&row, whole);
}

/// The remote-access filter reads `Policy`, so it must run before the strip: a
/// remote login screen still loses the fixture's admin, which refuses remote
/// access, and keeps bob, saved here as the one account that allows it.
#[tokio::test]
async fn the_remote_access_filter_reads_policy_before_it_is_stripped() {
    let users = Arc::new(MemUsers::default());
    users
        .echo_saved_policy
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *users.updated_policy.lock().unwrap() = Some((
        BOB_ID,
        UserPolicy {
            enable_remote_access: true,
            ..UserPolicy::default()
        },
    ));
    let names = async |peer: &str| {
        let body = get_as(users.clone(), true, "/Users/Public", None, peer).await;
        let rows = body.as_array().unwrap().clone();
        for row in &rows {
            assert_private_fields(row, false);
        }
        rows.iter()
            .map(|row| row["Name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names("127.0.0.1:9000").await, ["admin", "bob"]);
    assert_eq!(names("203.0.113.8:9000").await, ["bob"]);
}

/// The hidden and disabled filters read `Policy` too, so a stripped row must not
/// reach them: the login screen would list hidden and disabled accounts.
#[rstest::rstest]
#[case::hidden(UserPolicy { is_hidden: true, ..UserPolicy::default() })]
#[case::disabled(UserPolicy { is_disabled: true, ..UserPolicy::default() })]
#[tokio::test]
async fn the_login_screen_filters_read_policy_before_it_is_stripped(#[case] policy: UserPolicy) {
    let users = Arc::new(MemUsers::default());
    users
        .echo_saved_policy
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *users.updated_policy.lock().unwrap() = Some((BOB_ID, policy));
    let body = get_as(users, true, "/Users/Public", None, "127.0.0.1:9000").await;
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["Name"], "admin");
    assert_private_fields(&rows[0], false);
}

/// Upstream's `OrderBy(u => u.Username)` compares by culture, where case does
/// not come first: an ordinal sort would put `Zoe` before `admin`.
#[tokio::test]
async fn user_lists_sort_names_by_culture() {
    let users = Arc::new(MemUsers {
        listed_first: Some(user_entity(Uuid::from_u128(0x0E5E), "Zoe", None)),
        ..MemUsers::default()
    });
    let body = get_as(users, true, "/Users", Some("admin"), "127.0.0.1:9000").await;
    let names: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["admin", "bob", "Zoe"]);
}
