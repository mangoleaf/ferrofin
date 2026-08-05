//! Users — CRUD, policy, forgot-password, AuthenticateByName, Users/Me.
//!
//! Drives the user-admin, account-recovery, and current-user handlers through
//! `create_router` with injected `hermit-traits` fakes that authenticate and
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
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAuthContext, FakeConfig, FakeDto, FakeLibrary, FakeMediaSources, FakeMusic, FakeProviders,
    FakeSearch, FakeSimilarItems, FakeSystem, FakeUserData, FakeUserViews,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::security::DeviceEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::{ServerConfiguration, UserConfiguration};
use hermit_model::dto::{BaseItemDto, MediaSourceInfo, NameIdPair, SessionInfoDto, UserDto};
use hermit_model::querying::QueryResult;
use hermit_model::quick_connect::QuickConnectResult;
use hermit_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use hermit_model::users::UserPolicy;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, MediaSourceManager, UserManager, UserViewManager};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::security::QuickConnect;
use hermit_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use hermit_traits::system::ServerApplicationPaths;
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

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);

/// Builds a minimal [`UserEntity`] carrying the given id + username; every other
/// field is a neutral zero value ([`UserEntity`] has no `Default`).
fn hsp_user_entity(id: Uuid, username: &str) -> UserEntity {
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
        password: Some("hashed".to_owned()),
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

/// An [`AuthService`] that always authenticates as [`USER_ID`], so `RequireAuth`
/// yields an authenticated context in the success-path tests.
struct OkAuthService;

#[async_trait]
impl AuthService for OkAuthService {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(hsp_user_entity(USER_ID, "alice")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// An [`AuthorizationContext`] that resolves the same authenticated user, so
/// handlers reading the request extension (e.g. `AuthenticateByName`'s client
/// identity) see a populated context.
struct OkAuthContext;

#[async_trait]
impl AuthorizationContext for OkAuthContext {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(hsp_user_entity(USER_ID, "alice")),
            client: Some("Wolphin".to_owned()),
            version: Some("1.0".to_owned()),
            device_id: Some("dev-1".to_owned()),
            device: Some("Test Device".to_owned()),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
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
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
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
    ) -> Result<Vec<hermit_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_password_reset_providers(
        &self,
    ) -> Result<Vec<hermit_model::dto::NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_dto(
        &self,
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<hermit_model::dto::UserDto, ServiceError> {
        Ok(hermit_model::dto::UserDto {
            id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
            name: Some(user.username.clone()),
            server_id,
            ..hermit_model::dto::UserDto::default()
        })
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &hermit_model::configuration::UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &hermit_model::users::UserPolicy,
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
        original_language: None,
        original_title: None,
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
        _collection_type: hermit_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn create_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
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
        _options: &hermit_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
        Ok(hermit_model::dto::ItemCounts {
            movie_count: 3,
            series_count: 1,
            ..hermit_model::dto::ItemCounts::default()
        })
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: hermit_model::entities::MediaStreamType,
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
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(user_id).await
    }
    async fn get_latest_items(
        &self,
        _user_id: Uuid,
        _options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
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
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unimplemented!()
    }
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
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
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
    ) -> Result<Vec<hermit_model::entities_media::MediaStream>, ServiceError> {
        unimplemented!()
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<hermit_model::entities_media::MediaAttachment>, ServiceError> {
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
        _request: &hermit_model::media_info::LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!()
    }
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`SessionManager`] whose `authenticate_new_session` returns a canned
/// session for [`USER_ID`]; every other method is unused.
struct OkSessions;

#[async_trait]
impl SessionManager for OkSessions {
    async fn authenticate_new_session(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
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
        _user: &UserEntity,
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
        _info: &hermit_model::session::PlaybackStartInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn on_playback_progress(
        &self,
        _info: &hermit_model::session::PlaybackProgressInfo,
        _is_automated: bool,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn on_playback_stopped(
        &self,
        _info: &hermit_model::session::PlaybackStopInfo,
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
        _command: &hermit_model::session::GeneralCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &hermit_model::session::MessageCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_play_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &hermit_model::session::PlayRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_playstate_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &hermit_model::session::PlaystateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_admin_sessions(
        &self,
        _message_type: hermit_model::session::SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_user_sessions(
        &self,
        _user_ids: &[Uuid],
        _message_type: hermit_model::session::SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_user_device_sessions(
        &self,
        _device_id: &str,
        _message_type: hermit_model::session::SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
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
        _capabilities: &hermit_model::session::ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn report_transcoding_info(
        &self,
        _device_id: &str,
        _info: &hermit_model::session::TranscodingInfo,
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
        _device: &hermit_db::entities::security::DeviceEntity,
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
    AppState::new(
        Arc::new(OkLibrary { item_id }),
        Arc::new(OkUsers),
        Arc::new(OkUserViews { item_id }),
        Arc::new(FakeUserData),
        Arc::new(OkMediaSources {
            path: media_path.to_owned(),
        }),
        Arc::new(OkSessions),
        Arc::new(FakeSystem),
        // FakeAppHost is fine — handlers under test never call it.
        Arc::new(hermit_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(hermit_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(OkAuthContext),
        Arc::new(OkAuthService),
        Arc::new(hermit_api::test_support::FakeQuickConnect),
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
async fn forgot_password_unknown_user_reports_contact_admin() {
    // An unknown username must not disclose account existence, and touches no
    // filesystem (the user lookup returns `None` before a pin is issued).
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
                    serde_json::json!({ "EnteredUsername": "nobody" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["Action"], "ContactAdmin");
}

#[tokio::test]
async fn forgot_password_known_user_issues_and_redeems_pin() {
    let users = Arc::new(MemUsers::default());
    let router = create_router(state(users.clone(), Arc::new(MemConfig::new(true))));

    // 1) Request a pin for a known user → PinCode + a real pin file.
    let response = router
        .clone()
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
    let body = body_json(response).await;
    assert_eq!(body["Action"], "PinCode");
    let pin_file = body["PinFile"].as_str().expect("pin file path").to_owned();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pin_file).unwrap()).unwrap();
    let pin = record["Pin"].as_str().unwrap().to_owned();

    // 2) Redeem the pin → success, bob reset, password set to the pin, file gone.
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/ForgotPassword/Pin")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "Pin": pin }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["Success"], true);
    assert_eq!(body["UsersReset"][0], "bob");

    let (id, pw) = users.changed_password.lock().unwrap().clone().unwrap();
    assert_eq!(id, BOB_ID);
    assert_eq!(pw, pin);
    assert!(!std::path::Path::new(&pin_file).exists());
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
    assert_eq!(json["User"]["Id"], USER_ID.to_string());
    assert_eq!(json["User"]["Name"], "alice");
    assert_eq!(json["ServerId"], "server-1");
    // ...and the minted access token the client must present on later requests.
    assert_eq!(json["AccessToken"], "canned-token");
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
    assert_eq!(json["Id"], USER_ID.to_string());
    assert_eq!(json["Name"], "alice");
    assert_eq!(json["HasPassword"], true);
}
