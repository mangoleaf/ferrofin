//! Media info domain integration tests: `GET`/`POST /Items/{id}/PlaybackInfo`
//! returning the item's `MediaSources`.
//!
//! Ported from `handler_success_paths.rs`. The `ok_state` harness wires a library
//! resolving one fixed item and a media-source manager returning one source with
//! a known on-disk path, so the PlaybackInfo body shape can be asserted.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeConfig, FakeMusic, FakeProviders, FakeSearch, FakeSimilarItems, FakeSystem, FakeUserData,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{BaseItemDto, MediaSourceInfo, SessionInfoDto};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, MediaSourceManager, UserManager, UserViewManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use tower::ServiceExt;
use uuid::Uuid;

// A fixed authenticated user id shared across the stubs and the assertions.
const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);

/// Builds a minimal [`UserEntity`] carrying the given id + username.
fn user_entity(id: Uuid, username: &str) -> UserEntity {
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
        normalized_username: username.to_uppercase(),
    }
}

/// An [`AuthService`] that always authenticates as [`USER_ID`].
struct OkAuthService;

#[async_trait]
impl AuthService for OkAuthService {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// An [`AuthorizationContext`] that resolves the same authenticated user.
struct OkAuthContext;

#[async_trait]
impl AuthorizationContext for OkAuthContext {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            client: Some("Wolphin".to_owned()),
            version: Some("1.0".to_owned()),
            device_id: Some("dev-1".to_owned()),
            device: Some("Test Device".to_owned()),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`UserManager`] whose `get_user_by_id` returns the fixed user.
struct OkUsers;

#[async_trait]
impl UserManager for OkUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(|| user_entity(USER_ID, "alice")))
    }
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

/// Builds a minimal [`BaseItemEntity`] with the given id + a fixed name.
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

/// A [`LibraryManager`] resolving a single known item id (any other is `None`).
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
        unimplemented!()
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
        unimplemented!()
    }
    async fn delete_item(
        &self,
        _id: Uuid,
        _options: &ferrofin_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
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
        unimplemented!()
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
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(user_id).await
    }
    async fn get_latest_items(
        &self,
        _query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        unimplemented!()
    }
}

/// A [`DtoService`] projecting each entity into id + name.
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
    /// The last request `open_live_stream` was called with, so a test can read
    /// back the user id the handler resolved.
    opened: Mutex<Option<ferrofin_model::media_info::LiveStreamRequest>>,
}

fn media_source(path: &str) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some("source-1".to_owned()),
        path: Some(path.to_owned()),
        ..MediaSourceInfo::default()
    }
}

/// An opened live stream, as Live TV hands it back: HTTP, with its id.
fn live_source(path: &str) -> MediaSourceInfo {
    MediaSourceInfo {
        live_stream_id: Some("live-1".to_owned()),
        protocol: ferrofin_model::media_info::MediaProtocol::Http,
        ..media_source(path)
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
        request: &ferrofin_model::media_info::LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        *self.opened.lock().expect("lock") = Some(request.clone());
        Ok(live_source(&self.path))
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        Ok(live_source(&self.path))
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`SessionManager`] whose `authenticate_new_session` returns a canned session.
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
        unimplemented!()
    }
    async fn send_message_to_user_device_sessions(
        &self,
        _device_id: &str,
        _message_type: ferrofin_model::session::SessionMessageType,
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

/// Assembles an [`AppState`] wired for the media-info paths.
fn ok_state(item_id: Uuid, media_path: &str) -> AppState {
    ok_state_full(
        item_id,
        Arc::new(OkMediaSources {
            path: media_path.to_owned(),
            opened: Mutex::new(None),
        }),
        Arc::new(OkUsers),
    )
}

/// [`ok_state`] with the media-source fake and the caller's role chosen by the
/// test: [`OkUsers`] reports an ordinary account, `FakeAdminUsers` an
/// administrator.
fn ok_state_full(
    item_id: Uuid,
    media_sources: Arc<OkMediaSources>,
    users: Arc<dyn ferrofin_traits::library::UserManager>,
) -> AppState {
    ok_state_with_auth(
        item_id,
        media_sources,
        users,
        Arc::new(OkAuthContext),
        Arc::new(OkAuthService),
    )
}

fn ok_state_with_auth(
    item_id: Uuid,
    media_sources: Arc<OkMediaSources>,
    users: Arc<dyn ferrofin_traits::library::UserManager>,
    auth_context: Arc<dyn AuthorizationContext>,
    auth_service: Arc<dyn AuthService>,
) -> AppState {
    ok_state_parts(
        item_id,
        media_sources,
        users,
        auth_context,
        auth_service,
        Arc::new(ferrofin_api::test_support::FakeDevices),
    )
}

/// [`ok_state_with_auth`] with any media-source and device manager.
fn ok_state_parts(
    item_id: Uuid,
    media_sources: Arc<dyn MediaSourceManager>,
    users: Arc<dyn ferrofin_traits::library::UserManager>,
    auth_context: Arc<dyn AuthorizationContext>,
    auth_service: Arc<dyn AuthService>,
    devices: Arc<dyn ferrofin_traits::devices::DeviceManager>,
) -> AppState {
    AppState::new(
        Arc::new(OkLibrary { item_id }),
        users,
        Arc::new(OkUserViews { item_id }),
        Arc::new(FakeUserData),
        media_sources,
        Arc::new(OkSessions),
        Arc::new(FakeSystem),
        Arc::new(ferrofin_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        auth_context,
        auth_service,
        Arc::new(ferrofin_api::test_support::FakeQuickConnect),
        Arc::new(ferrofin_api::test_support::FakePlaylists),
        Arc::new(ferrofin_api::test_support::FakeCollections),
        Arc::new(ferrofin_api::test_support::FakeTvSeries),
        Arc::new(ferrofin_api::test_support::FakeSubtitles),
        Arc::new(ferrofin_api::test_support::FakeLyrics),
        Arc::new(ferrofin_api::test_support::FakeMediaSegments),
        Arc::new(ferrofin_api::test_support::FakeTrickplay),
        devices,
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
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
async fn playback_info_get_returns_media_sources() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, "/tmp/ferrofin-test-media.mp4"));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/PlaybackInfo"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["MediaSources"][0]["Id"], "source-1");
    assert_eq!(
        json["MediaSources"][0]["Path"],
        "/tmp/ferrofin-test-media.mp4"
    );
}

#[tokio::test]
async fn playback_info_post_returns_media_sources() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, "/tmp/ferrofin-test-media.mp4"));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}/PlaybackInfo"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"DeviceProfile":{}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["MediaSources"][0]["Id"], "source-1");
}

#[tokio::test]
async fn api_key_playback_info_only_requires_a_user_for_profile_negotiation() {
    let item_id = Uuid::from_u128(0xABCD);
    for (body, expected) in [
        (serde_json::json!({}), StatusCode::OK),
        (
            serde_json::json!({"DeviceProfile": {}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"DeviceProfile": {}, "UserId": USER_ID}),
            StatusCode::OK,
        ),
        (
            serde_json::json!({"DeviceProfile": {}, "UserId": Uuid::from_u128(999)}),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let app = ok_state_with_auth(
            item_id,
            Arc::new(OkMediaSources {
                path: "/tmp/ferrofin-test-media.mp4".to_owned(),
                opened: Mutex::new(None),
            }),
            Arc::new(OkUsers),
            Arc::new(ferrofin_api::test_support::FakeAuthContext),
            Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
        );
        let response = create_router(app)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/Items/{item_id}/PlaybackInfo"))
                    .header("X-Emby-Token", "valid")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{body}");
    }
}

#[tokio::test]
async fn posted_playback_user_is_authorized_and_query_takes_precedence() {
    let item_id = Uuid::from_u128(0xABCD);
    for (query, expected) in [
        (String::new(), StatusCode::FORBIDDEN),
        (format!("?userId={USER_ID}"), StatusCode::OK),
    ] {
        let response = create_router(ok_state(item_id, "/tmp/ferrofin-test-media.mp4"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/Items/{item_id}/PlaybackInfo{query}"))
                    .header("X-Emby-Token", "valid")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"UserId": Uuid::from_u128(999)}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{query}");
    }
}

/// `POST /LiveStreams/Open` folds `?userId=` and the body's `UserId` and then
/// runs C# `RequestHelpers.GetUserId` on the result
/// (v10.11.8 `MediaInfoController.cs`), so a non-administrator naming another
/// user is refused. The resolved id is what the stream is opened AS — and for a
/// Live TV source that is what opens the tuner — so an ungated one let any
/// account open a stream under another identity.
#[tokio::test]
async fn open_live_stream_cross_user_as_non_admin_is_forbidden() {
    let other = Uuid::from_u128(0x0BAD);
    for uri in [
        format!("/LiveStreams/Open?userId={other}"),
        "/LiveStreams/Open".to_owned(),
    ] {
        let ms = Arc::new(OkMediaSources {
            path: "/tmp/x.mkv".to_owned(),
            opened: Mutex::new(None),
        });
        let app = ok_state_full(Uuid::from_u128(0x0177), ms.clone(), Arc::new(OkUsers));
        // The second leg carries the id in the BODY, which C# folds in first.
        let body = if uri.contains("userId") {
            r#"{"OpenToken":"tok"}"#.to_owned()
        } else {
            format!(r#"{{"OpenToken":"tok","UserId":"{other}"}}"#)
        };
        let response = create_router(app)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header("Authorization", "Bearer t")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
        assert!(ms.opened.lock().expect("lock").is_none(), "{uri}");
    }
}

/// An administrator may still open a stream on another user's behalf, and the
/// request carries THAT user's id.
#[tokio::test]
async fn open_live_stream_cross_user_as_admin_is_allowed() {
    let other = Uuid::from_u128(0x0BAD);
    let ms = Arc::new(OkMediaSources {
        path: "/tmp/x.mkv".to_owned(),
        opened: Mutex::new(None),
    });
    let app = ok_state_full(
        Uuid::from_u128(0x0177),
        ms.clone(),
        Arc::new(ferrofin_api::test_support::FakeAdminUsers),
    );
    let response = create_router(app)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/LiveStreams/Open?userId={other}"))
                .header("Authorization", "Bearer t")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"OpenToken":"tok"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        ms.opened
            .lock()
            .expect("lock")
            .as_ref()
            .expect("opened")
            .user_id,
        other
    );
}

/// A non-administrator naming their own id is served, and the request carries
/// their id — the first half of the rule still applies.
#[tokio::test]
async fn open_live_stream_self_as_non_admin_is_allowed() {
    let ms = Arc::new(OkMediaSources {
        path: "/tmp/x.mkv".to_owned(),
        opened: Mutex::new(None),
    });
    let app = ok_state_full(Uuid::from_u128(0x0177), ms.clone(), Arc::new(OkUsers));
    let response = create_router(app)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/LiveStreams/Open?userId={USER_ID}"))
                .header("Authorization", "Bearer t")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"OpenToken":"tok"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        ms.opened
            .lock()
            .expect("lock")
            .as_ref()
            .expect("opened")
            .user_id,
        USER_ID
    );
}

/// Live TV's shared buffer URL, built on the loopback address for the
/// server's own readers (its ffmpeg and the DVR)…
const LOOPBACK_BUFFER: &str = "http://127.0.0.1:8096/LiveTv/LiveStreamFiles/abc/stream.ts";
/// …and the same URL at the address this request reached the server by
/// ([`ferrofin_api::test_support::FakeAppHost`]'s smart API URL).
const PUBLISHED_BUFFER: &str = "http://localhost:8096/LiveTv/LiveStreamFiles/abc/stream.ts";

/// `MediaInfoHelper.OpenMediaSource` → `RewritePublishedLiveStreamPath`
/// (v12.2): the opened stream's response copy points its buffer URL at the
/// request's smart API URL, not at the client's own loopback.
#[tokio::test]
async fn open_live_stream_publishes_the_buffer_url_at_the_request_url() {
    let app = ok_state(Uuid::from_u128(0x0177), LOOPBACK_BUFFER);
    let response = create_router(app)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/LiveStreams/Open?userId={USER_ID}"))
                .header("Authorization", "Bearer t")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"OpenToken":"tok"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["MediaSource"]["Path"],
        PUBLISHED_BUFFER
    );
}

/// `MediaInfoHelper.GetPlaybackInfo` rewrites the cloned sources the same
/// way: an already-open live stream named by `liveStreamId` is published at
/// the request's URL.
#[tokio::test]
async fn playback_info_publishes_an_open_live_stream_buffer_url() {
    let item_id = Uuid::from_u128(0xABCD);
    let response = create_router(ok_state(item_id, LOOPBACK_BUFFER))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}/PlaybackInfo?liveStreamId=live-1"))
                .header("X-Emby-Token", "valid")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["MediaSources"][0]["LiveStreamId"], "live-1");
    assert_eq!(json["MediaSources"][0]["Path"], PUBLISHED_BUFFER);
}

/// A [`MediaSourceManager`] serving a fixed set of versions and any open live
/// stream by id, and recording what `open_live_stream` was asked to open.
struct VersionedSources {
    sources: Vec<MediaSourceInfo>,
    opened: Mutex<Option<ferrofin_model::media_info::LiveStreamRequest>>,
    /// The source an open hands back, when a test needs a real one.
    open_as: Option<MediaSourceInfo>,
}

#[async_trait]
impl MediaSourceManager for VersionedSources {
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
        Ok(self.sources.clone())
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        Ok(self.sources.clone())
    }
    async fn open_live_stream(
        &self,
        request: &ferrofin_model::media_info::LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        *self.opened.lock().expect("lock") = Some(request.clone());
        if let Some(source) = &self.open_as {
            return Ok(source.clone());
        }
        Ok(MediaSourceInfo {
            id: Some("opened".to_owned()),
            live_stream_id: Some("live-1".to_owned()),
            protocol: ferrofin_model::media_info::MediaProtocol::Http,
            path: Some(LOOPBACK_BUFFER.to_owned()),
            ..MediaSourceInfo::default()
        })
    }
    async fn get_live_stream(&self, id: &str) -> Result<MediaSourceInfo, ServiceError> {
        Ok(MediaSourceInfo {
            id: Some(format!("open-{id}")),
            live_stream_id: Some(id.to_owned()),
            ..MediaSourceInfo::default()
        })
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// An h264 mp4 version with two `audio` tracks (1, 2) and a text subtitle (3).
fn version(id: &str, audio: &str) -> MediaSourceInfo {
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_model::entities_media::MediaStream;
    let stream = |stream_type, index, codec: &str| MediaStream {
        stream_type,
        index,
        codec: Some(codec.to_owned()),
        ..MediaStream::default()
    };
    MediaSourceInfo {
        id: Some(id.to_owned()),
        path: Some(format!("/media/{id}.mp4")),
        container: Some("mp4".to_owned()),
        bitrate: Some(8_000_000),
        supports_direct_play: true,
        supports_direct_stream: true,
        supports_transcoding: true,
        default_audio_stream_index: Some(1),
        media_streams: vec![
            stream(MediaStreamType::Video, 0, "h264"),
            stream(MediaStreamType::Audio, 1, audio),
            stream(MediaStreamType::Audio, 2, audio),
            stream(MediaStreamType::Subtitle, 3, "subrip"),
        ],
        ..MediaSourceInfo::default()
    }
}

/// A browser-like profile: direct-plays h264/aac mp4, transcodes to HLS ts.
fn web_profile() -> serde_json::Value {
    serde_json::json!({
        "DirectPlayProfiles": [
            {"Container": "mp4", "VideoCodec": "h264", "AudioCodec": "aac", "Type": "Video"}
        ],
        "TranscodingProfiles": [
            {"Container": "ts", "Type": "Video", "VideoCodec": "h264", "AudioCodec": "aac",
             "Protocol": "hls", "Context": "Streaming"}
        ],
        "SubtitleProfiles": [{"Format": "vtt", "Method": "External"}]
    })
}

fn versioned(sources: Vec<MediaSourceInfo>) -> Arc<VersionedSources> {
    Arc::new(VersionedSources {
        sources,
        opened: Mutex::new(None),
        open_as: None,
    })
}

/// [`versioned`] whose opens hand back `open_as`.
fn versioned_opening(
    sources: Vec<MediaSourceInfo>,
    open_as: MediaSourceInfo,
) -> Arc<VersionedSources> {
    Arc::new(VersionedSources {
        sources,
        opened: Mutex::new(None),
        open_as: Some(open_as),
    })
}

/// An opened live stream with tracks: [`version`] `opened` (h264 video,
/// `audio` tracks) carrying a live-stream id.
fn opened_live(audio: &str) -> MediaSourceInfo {
    MediaSourceInfo {
        live_stream_id: Some("live-1".to_owned()),
        ..version("opened", audio)
    }
}

/// POSTs `body` to `/LiveStreams/Open` as [`USER_ID`] with `state`.
async fn open_response(state: AppState, body: &serde_json::Value) -> axum::response::Response {
    create_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/LiveStreams/Open?userId={USER_ID}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// [`open_response`], expecting `200 OK`, as JSON.
async fn post_open(state: AppState, body: &serde_json::Value) -> serde_json::Value {
    let response = open_response(state, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    json_body(response).await
}

/// POSTs `body` (or no body) to the item's PlaybackInfo with `state`.
async fn post_playback(
    state: AppState,
    item_id: Uuid,
    body: Option<&serde_json::Value>,
) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/Items/{item_id}/PlaybackInfo"))
        .header("X-Emby-Token", "valid");
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    };
    let response = create_router(state)
        .oneshot(request.unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    json_body(response).await
}

/// An [`AppState`] over `sources` and `devices`.
fn versions_state(
    item_id: Uuid,
    sources: Arc<VersionedSources>,
    devices: Arc<dyn ferrofin_traits::devices::DeviceManager>,
) -> AppState {
    ok_state_parts(
        item_id,
        sources,
        Arc::new(OkUsers),
        Arc::new(OkAuthContext),
        Arc::new(OkAuthService),
        devices,
    )
}

/// POSTs `body` to the item's PlaybackInfo against `sources`.
async fn post_versions(
    item_id: Uuid,
    sources: Arc<VersionedSources>,
    body: &serde_json::Value,
) -> serde_json::Value {
    let state = versions_state(
        item_id,
        sources,
        Arc::new(ferrofin_api::test_support::FakeDevices),
    );
    post_playback(state, item_id, Some(body)).await
}

#[tokio::test]
async fn posted_media_source_id_returns_only_that_version_with_the_picked_tracks() {
    // jellyfin-web's version picker posts the alternate's id with the tracks
    // chosen on the details page; upstream ResolvePlaybackMediaSources returns
    // that version alone, so the client cannot fall back to the primary.
    let item_id = Uuid::from_u128(0xABCD);
    let primary = item_id.simple().to_string();
    let json = post_versions(
        item_id,
        versioned(vec![version(&primary, "aac"), version("alt", "aac")]),
        &serde_json::json!({
            "DeviceProfile": web_profile(),
            "MediaSourceId": "ALT",
            "AudioStreamIndex": 2,
            "SubtitleStreamIndex": -1,
        }),
    )
    .await;
    let sources = json["MediaSources"].as_array().expect("sources");
    assert_eq!(sources.len(), 1, "{json}");
    assert_eq!(sources[0]["Id"], "alt");
    assert_eq!(sources[0]["DefaultAudioStreamIndex"], 2);
    // "Off" stays off rather than falling back to the default subtitle.
    assert_eq!(sources[0]["DefaultSubtitleStreamIndex"], -1);
}

#[tokio::test]
async fn track_picks_without_a_media_source_id_apply_to_no_version() {
    // SetDeviceSpecificData applies the picks only to the version whose id
    // matches; with none named every version keeps its default track.
    let item_id = Uuid::from_u128(0xABCD);
    let primary = item_id.simple().to_string();
    let json = post_versions(
        item_id,
        versioned(vec![version("alt", "aac"), version(&primary, "aac")]),
        &serde_json::json!({"DeviceProfile": web_profile(), "AudioStreamIndex": 2}),
    )
    .await;
    let sources = json["MediaSources"].as_array().expect("sources");
    assert_eq!(sources.len(), 2);
    // SortMediaSources puts the queried item's own version first.
    assert_eq!(sources[0]["Id"], primary.as_str());
    assert!(
        sources.iter().all(|s| s["DefaultAudioStreamIndex"] == 1),
        "{json}"
    );
}

#[tokio::test]
async fn unknown_media_source_id_reports_no_compatible_stream() {
    let item_id = Uuid::from_u128(0xABCD);
    let json = post_versions(
        item_id,
        versioned(vec![version("alt", "aac")]),
        &serde_json::json!({"MediaSourceId": "missing"}),
    )
    .await;
    assert_eq!(json["MediaSources"], serde_json::json!([]));
    assert_eq!(json["ErrorCode"], "NoCompatibleStream");
    assert!(json.get("PlaySessionId").is_none(), "{json}");
}

#[tokio::test]
async fn live_stream_id_plays_the_open_stream() {
    let item_id = Uuid::from_u128(0xABCD);
    let json = post_versions(
        item_id,
        versioned(vec![version("alt", "aac")]),
        &serde_json::json!({"LiveStreamId": "live-7"}),
    )
    .await;
    assert_eq!(json["MediaSources"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["MediaSources"][0]["Id"], "open-live-7");
}

#[tokio::test]
async fn auto_open_live_stream_opens_a_source_that_requires_opening() {
    let item_id = Uuid::from_u128(0xABCD);
    let channel = MediaSourceInfo {
        id: Some("channel".to_owned()),
        requires_opening: true,
        open_token: Some("tok".to_owned()),
        ..MediaSourceInfo::default()
    };
    let sources = versioned(vec![channel]);
    let json = post_versions(
        item_id,
        Arc::clone(&sources),
        &serde_json::json!({"AutoOpenLiveStream": true, "StartTimeTicks": 5, "MaxAudioChannels": 2}),
    )
    .await;
    assert_eq!(json["MediaSources"][0]["Id"], "opened");
    // Published as `POST /LiveStreams/Open` publishes it (`OpenMediaSource`).
    assert_eq!(json["MediaSources"][0]["Path"], PUBLISHED_BUFFER);
    let opened = sources
        .opened
        .lock()
        .expect("lock")
        .clone()
        .expect("opened");
    assert_eq!(opened.open_token.as_deref(), Some("tok"));
    assert_eq!(opened.user_id, USER_ID);
    assert_eq!(opened.item_id, item_id);
    assert_eq!(opened.start_time_ticks, Some(5));
    assert_eq!(opened.max_audio_channels, Some(2));
    assert_eq!(
        opened.play_session_id.as_deref(),
        json["PlaySessionId"].as_str()
    );
}

/// A [`DeviceManager`](ferrofin_traits::devices::DeviceManager) whose stored
/// capabilities carry the web profile, recording which device was asked for.
struct ProfileDevices {
    asked: Mutex<Option<String>>,
}

#[async_trait]
impl ferrofin_traits::devices::DeviceManager for ProfileDevices {
    async fn create_device(
        &self,
        _device: &ferrofin_db::entities::security::DeviceEntity,
    ) -> Result<ferrofin_db::entities::security::DeviceEntity, ServiceError> {
        unimplemented!()
    }
    async fn save_capabilities(
        &self,
        _device_id: &str,
        _capabilities: &ferrofin_model::session::ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_capabilities(
        &self,
        device_id: Option<&str>,
    ) -> Result<ferrofin_model::session::ClientCapabilities, ServiceError> {
        *self.asked.lock().expect("lock") = device_id.map(str::to_owned);
        Ok(ferrofin_model::session::ClientCapabilities {
            device_profile: Some(serde_json::from_value(web_profile()).expect("profile")),
            ..ferrofin_model::session::ClientCapabilities::default()
        })
    }
    async fn get_device(
        &self,
        _id: &str,
    ) -> Result<Option<ferrofin_model::dto::DeviceInfoDto>, ServiceError> {
        unimplemented!()
    }
    async fn get_devices(
        &self,
        _query: &ferrofin_traits::devices::DeviceQuery,
    ) -> Result<QueryResult<ferrofin_db::entities::security::DeviceEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_device_infos(
        &self,
        _query: &ferrofin_traits::devices::DeviceQuery,
    ) -> Result<QueryResult<ferrofin_model::devices::DeviceInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_devices_for_user(
        &self,
        _user_id: Option<Uuid>,
    ) -> Result<QueryResult<ferrofin_model::dto::DeviceInfoDto>, ServiceError> {
        unimplemented!()
    }
    async fn delete_device(
        &self,
        _device: &ferrofin_db::entities::security::DeviceEntity,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_device(
        &self,
        _device: &ferrofin_db::entities::security::DeviceEntity,
    ) -> Result<(), ServiceError> {
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
        _device_id: &str,
        _device_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_device_options(
        &self,
        _device_id: &str,
    ) -> Result<Option<ferrofin_db::entities::security::DeviceOptionsEntity>, ServiceError> {
        unimplemented!()
    }
    async fn to_client_capabilities_dto(
        &self,
        _capabilities: &ferrofin_model::session::ClientCapabilities,
    ) -> Result<ferrofin_model::dto::ClientCapabilitiesDto, ServiceError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn a_post_without_a_profile_negotiates_with_the_stored_capabilities() {
    // AC3 cannot direct-play under the stored web profile, so a negotiated
    // response clears SupportsDirectPlay; the static source would keep it.
    let item_id = Uuid::from_u128(0xABCD);
    let devices = Arc::new(ProfileDevices {
        asked: Mutex::new(None),
    });
    let state = versions_state(
        item_id,
        versioned(vec![version("alt", "ac3")]),
        Arc::clone(&devices) as Arc<dyn ferrofin_traits::devices::DeviceManager>,
    );
    let json = post_playback(state, item_id, None).await;
    assert_eq!(
        devices.asked.lock().expect("lock").as_deref(),
        Some("dev-1")
    );
    assert_eq!(
        json["MediaSources"][0]["SupportsDirectPlay"], false,
        "{json}"
    );
}

/// `MediaInfoHelper.OpenMediaSource` (v12.2): with a posted `DeviceProfile`,
/// the opened source is negotiated as PlaybackInfo negotiates — here AC3
/// audio the profile cannot play loses the source its direct play (it kept
/// `SupportsDirectPlay: true` before; the test user's policy permits no
/// transcode, so no transcoding URL is built, as in the PlaybackInfo tests).
#[tokio::test]
async fn open_live_stream_negotiates_the_opened_source_against_the_posted_profile() {
    let item_id = Uuid::from_u128(0xABCD);
    let sources = versioned_opening(vec![version("v1", "aac")], opened_live("ac3"));
    let state = versions_state(
        item_id,
        sources,
        Arc::new(ferrofin_api::test_support::FakeDevices),
    );
    let json = post_open(
        state,
        &serde_json::json!({"OpenToken": "tok", "ItemId": item_id, "DeviceProfile": web_profile()}),
    )
    .await;
    let source = &json["MediaSource"];
    assert_eq!(source["SupportsDirectPlay"], false, "{source}");
}

/// Without a profile (none posted, none stored for the device), an opened
/// source's existing transcoding URL is tagged with its live stream.
#[tokio::test]
async fn open_live_stream_without_a_profile_tags_the_transcoding_url_with_its_live_stream() {
    let item_id = Uuid::from_u128(0xABCD);
    let opened = MediaSourceInfo {
        transcoding_url: Some("/videos/x/master.m3u8?MediaSourceId=opened".to_owned()),
        ..opened_live("aac")
    };
    let sources = versioned_opening(vec![version("v1", "aac")], opened);
    let state = versions_state(
        item_id,
        sources,
        Arc::new(ferrofin_api::test_support::FakeDevices),
    );
    let json = post_open(
        state,
        &serde_json::json!({"OpenToken": "tok", "ItemId": item_id}),
    )
    .await;
    assert_eq!(
        json["MediaSource"]["TranscodingUrl"],
        "/videos/x/master.m3u8?MediaSourceId=opened&LiveStreamId=live-1"
    );
}

/// PlaybackInfo's `AutoOpenLiveStream` opens through the same
/// `OpenMediaSource`, so the opened source is negotiated against the
/// posted profile too.
#[tokio::test]
async fn auto_open_negotiates_the_opened_source_against_the_posted_profile() {
    let item_id = Uuid::from_u128(0xABCD);
    let channel = MediaSourceInfo {
        id: Some("channel".to_owned()),
        requires_opening: true,
        open_token: Some("tok".to_owned()),
        ..MediaSourceInfo::default()
    };
    let sources = versioned_opening(vec![channel], opened_live("ac3"));
    let json = post_versions(
        item_id,
        sources,
        &serde_json::json!({"AutoOpenLiveStream": true, "DeviceProfile": web_profile()}),
    )
    .await;
    let source = &json["MediaSources"][0];
    assert_eq!(source["Id"], "opened");
    assert_eq!(source["SupportsDirectPlay"], false, "{source}");
}

/// With no profile posted, `OpenMediaSource` negotiates against the profile the
/// device last reported, and narrows the opened source's container list to the
/// one that profile plays (`NormalizeMediaSourceContainer`).
#[tokio::test]
async fn open_live_stream_negotiates_against_the_stored_profile_and_normalizes_the_container() {
    let item_id = Uuid::from_u128(0xABCD);
    let devices = Arc::new(ProfileDevices {
        asked: Mutex::new(None),
    });
    let opened = MediaSourceInfo {
        container: Some("mov,mp4,m4a".to_owned()),
        ..opened_live("ac3")
    };
    let state = versions_state(
        item_id,
        versioned_opening(vec![version("v1", "aac")], opened),
        Arc::clone(&devices) as Arc<dyn ferrofin_traits::devices::DeviceManager>,
    );
    let json = post_open(
        state,
        &serde_json::json!({"OpenToken": "tok", "ItemId": item_id}),
    )
    .await;
    assert_eq!(
        devices.asked.lock().expect("lock").as_deref(),
        Some("dev-1")
    );
    let source = &json["MediaSource"];
    assert_eq!(source["SupportsDirectPlay"], false, "{source}");
    assert_eq!(source["Container"], "mp4", "{source}");
}

/// Negotiating needs the item: an unknown `ItemId` with a profile is a 404
/// (upstream's `ResourceNotFoundException`).
#[tokio::test]
async fn open_live_stream_with_a_profile_for_an_unknown_item_is_not_found() {
    let item_id = Uuid::from_u128(0xABCD);
    let state = versions_state(
        item_id,
        versioned_opening(vec![version("v1", "aac")], opened_live("ac3")),
        Arc::new(ferrofin_api::test_support::FakeDevices),
    );
    let response = open_response(
        state,
        &serde_json::json!({
            "OpenToken": "tok",
            "ItemId": Uuid::from_u128(0xDEAD),
            "DeviceProfile": web_profile()
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
