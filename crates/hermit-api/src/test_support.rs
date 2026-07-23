//! Fake `hermit-traits` manager impls for testing `hermit-api` in isolation.
//!
//! These are minimal **test doubles**, not real implementations — `hermit-api`
//! must never dev-depend on `hermit-core`. Only the two authorization traits
//! carry meaningful behaviour (they are the ones the router's auth-context
//! middleware invokes); every other manager method is `unimplemented!()`, since
//! the INFRA-level tests exercise only routing, health, and auth, never the
//! domain managers.
//!
//! Gated behind the `test-util` feature so it is compiled for this crate's tests
//! (unit and integration) but never shipped in a production build.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use hermit_db::entities::security::DeviceEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::{ServerConfiguration, UserConfiguration};
use hermit_model::data::CollectionType;
use hermit_model::dto::{
    BaseItemDto, ItemCounts, MediaSourceInfo, NameIdPair, SessionInfoDto, UpdateUserItemDataDto,
    UserItemDataDto,
};
use hermit_model::entities_media::{MediaAttachment, MediaStream};
use hermit_model::media_info::LiveStreamRequest;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use hermit_model::system::{PublicSystemInfo, SystemInfo, SystemStorageInfo};
use hermit_model::users::UserPolicy;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{
    LibraryManager, MediaSourceManager, UserDataManager, UserManager, UserViewManager,
};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::persistence::ItemWithCounts;
use hermit_traits::session::{AuthenticationRequest, SessionManager};
use hermit_traits::system::{ServerApplicationHost, ServerApplicationPaths, SystemManager};
use uuid::Uuid;

use crate::state::AppState;

/// Builds an [`AppState`] whose every manager is a fake test double.
///
/// The authorization traits behave sensibly (anonymous context / unauthorized);
/// all other managers panic if called, which is exactly what the routing-level
/// tests want — they never reach a real domain manager.
#[must_use]
pub fn fake_state() -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(FakeAuthService),
    )
}

/// A fake [`AuthorizationContext`] that always resolves to an anonymous,
/// unauthenticated context (never rejecting), mirroring a request with no token.
pub struct FakeAuthContext;

#[async_trait]
impl AuthorizationContext for FakeAuthContext {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo::default())
    }
}

/// A fake [`AuthService`] that always rejects (no valid credentials), so
/// `RequireAuth`-guarded routes return `401` in tests.
pub struct FakeAuthService;

#[async_trait]
impl AuthService for FakeAuthService {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Err(ServiceError::unauthorized("no credentials"))
    }
}

/// A fake [`LibraryManager`]; every method is unused by INFRA-level tests.
pub struct FakeLibrary;

#[async_trait]
impl LibraryManager for FakeLibrary {
    async fn get_item_by_id(&self, _id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_list(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_item(&self, _id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_people_names(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_counts(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        unimplemented!("fake")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`UserManager`]; every method is unused by INFRA-level tests.
pub struct FakeUsers;

#[async_trait]
impl UserManager for FakeUsers {
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("fake")
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_by_id(&self, _id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn change_password(
        &self,
        _user_id: Uuid,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_user(
        &self,
        _username: &str,
        _password: &str,
        _remote_endpoint: &str,
        _is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`UserViewManager`]; every method is unused by INFRA-level tests.
pub struct FakeUserViews;

#[async_trait]
impl UserViewManager for FakeUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_latest_items(
        &self,
        _user_id: Uuid,
        _options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`UserDataManager`]; every method is unused by INFRA-level tests.
pub struct FakeUserData;

#[async_trait]
impl UserDataManager for FakeUserData {
    async fn save_user_data(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_data_dto(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_data_batch(
        &self,
        _item_ids: &[Uuid],
        _user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_play_state(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        unimplemented!("fake")
    }
    async fn reset_playback_stream_selections(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`MediaSourceManager`]; every method is unused by INFRA-level tests.
pub struct FakeMediaSources;

#[async_trait]
impl MediaSourceManager for FakeMediaSources {
    async fn get_media_streams(&self, _item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaAttachment>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_playback_media_sources(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        _enable_path_substitution: bool,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn open_live_stream(
        &self,
        _request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!("fake")
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`SessionManager`]; every method is unused by INFRA-level tests.
pub struct FakeSessions;

#[async_trait]
impl SessionManager for FakeSessions {
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
        unimplemented!("fake")
    }
    async fn close_live_stream_if_needed(
        &self,
        _live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`SystemManager`]. The info getters return a default value so the
/// now-real `/System/Info` and `/System/Info/Public` handlers resolve (the
/// contract probe expects them to route, not `404`); the lifecycle/storage
/// methods stay `unimplemented!` (never exercised by these tests).
pub struct FakeSystem;

#[async_trait]
impl SystemManager for FakeSystem {
    async fn get_system_info(&self, _request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        Ok(SystemInfo::default())
    }
    async fn get_public_system_info(
        &self,
        _request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        Ok(PublicSystemInfo::default())
    }
    async fn restart(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn shutdown(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_system_storage_info(&self) -> Result<SystemStorageInfo, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`ServerApplicationHost`]; getters return neutral values, URL builders
/// are unused by INFRA-level tests.
pub struct FakeAppHost;

#[async_trait]
impl ServerApplicationHost for FakeAppHost {
    fn core_startup_has_completed(&self) -> bool {
        true
    }
    fn http_port(&self) -> u16 {
        8096
    }
    fn https_port(&self) -> u16 {
        8920
    }
    fn listen_with_https(&self) -> bool {
        false
    }
    fn friendly_name(&self) -> String {
        "hermit-test".to_owned()
    }
    async fn get_smart_api_url(&self, _request: &RequestContext) -> Result<String, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_local_api_url(
        &self,
        _hostname: &str,
        _scheme: Option<&str>,
        _port: Option<u16>,
    ) -> Result<String, ServiceError> {
        unimplemented!("fake")
    }
    fn expand_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
    fn reverse_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
}

/// A fake [`ServerApplicationPaths`]; every accessor returns an empty path.
pub struct FakePaths;

impl ServerApplicationPaths for FakePaths {
    fn root_folder_path(&self) -> String {
        String::new()
    }
    fn default_user_views_path(&self) -> String {
        String::new()
    }
    fn people_path(&self) -> String {
        String::new()
    }
    fn genre_path(&self) -> String {
        String::new()
    }
    fn music_genre_path(&self) -> String {
        String::new()
    }
    fn studio_path(&self) -> String {
        String::new()
    }
    fn year_path(&self) -> String {
        String::new()
    }
    fn artists_path(&self) -> String {
        String::new()
    }
    fn user_configuration_directory_path(&self) -> String {
        String::new()
    }
    fn internal_metadata_path(&self) -> String {
        String::new()
    }
    fn program_data_path(&self) -> String {
        String::new()
    }
    fn web_path(&self) -> String {
        String::new()
    }
    fn data_path(&self) -> String {
        String::new()
    }
    fn image_cache_path(&self) -> String {
        String::new()
    }
    fn cache_path(&self) -> String {
        String::new()
    }
    fn log_directory_path(&self) -> String {
        String::new()
    }
}

/// A fake [`ServerConfigurationManager`]; `application_paths` yields [`FakePaths`],
/// the rest are unused by INFRA-level tests.
pub struct FakeConfig;

#[async_trait]
impl ServerConfigurationManager for FakeConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(FakePaths)
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`DtoService`]; every method is unused by INFRA-level tests.
pub struct FakeDto;

#[async_trait]
impl DtoService for FakeDto {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_base_item_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_base_item_dtos(
        &self,
        _items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!("fake")
    }
}
