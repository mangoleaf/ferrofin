//! First-Light handler **success-path** tests.
//!
//! Where `first_light.rs` proves the authenticated routes are guarded (`401`
//! without a token), this file drives each real handler all the way through with
//! stub `hermit-traits` impls that *authenticate* and *return data*, asserting
//! the success status and the wire-body shape:
//!
//! - `POST /Users/AuthenticateByName` → `200` + `AuthenticationResult`
//! - `GET  /Users/Me`                 → `200` + `UserDto`
//! - `GET  /Items`                    → `200` + `QueryResult<BaseItemDto>`
//! - `GET  /Items/{itemId}`           → `200` + `BaseItemDto`
//! - `GET  /Items/{itemId}` (missing) → `404`
//! - `GET  /UserViews`                → `200` + `QueryResult<BaseItemDto>`
//! - `GET`/`POST /Items/{itemId}/PlaybackInfo` → `200` + `MediaSources`
//! - `GET  /Videos/{itemId}/stream`   → `200` full body; `206` + `Content-Range`
//!   for a `Range` request
//! - `GET  /Items/{itemId}/Images/{imageType}` → `404` (no image processor yet),
//!   `400` for a garbage image type
//!
//! Every stub authenticates the request (so the `RequireAuth` extractor passes);
//! managers not exercised by a given handler reuse the `test_support` fakes,
//! which panic if touched — catching a handler that strays onto an unexpected
//! manager.

use std::io::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeConfig, FakeMusic, FakeSearch, FakeSimilarItems, FakeSystem, FakeUserData,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::dto::{BaseItemDto, MediaSourceInfo, SessionInfoDto};
use hermit_model::querying::QueryResult;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, MediaSourceManager, UserManager, UserViewManager};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use tower::ServiceExt;
use uuid::Uuid;

// A fixed authenticated user id shared across the stubs and the assertions.
const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);

/// Builds a minimal [`UserEntity`] carrying the given id + username; every other
/// field is a neutral zero value ([`UserEntity`] has no `Default`).
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
            user: Some(user_entity(USER_ID, "alice")),
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

/// A [`UserManager`] whose `get_user_by_id` returns the fixed user (any other id
/// yields `None`); every other method delegates to the panic fake by being
/// unused.
struct OkUsers;

#[async_trait]
impl UserManager for OkUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(|| user_entity(USER_ID, "alice")))
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
            access_token: "canned-token".to_owned(),
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
async fn system_info_authenticated_returns_body() {
    // `FakeSystem` returns a default `SystemInfo`; with the always-ok auth
    // service the `RequireAuth`-guarded handler runs and serializes it.
    let router = create_router(ok_state(Uuid::from_u128(1), ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // A well-formed JSON object body (SystemInfo) is returned.
    let json = json_body(response).await;
    assert!(json.is_object());
}

#[tokio::test]
async fn public_system_info_returns_body() {
    let router = create_router(ok_state(Uuid::from_u128(1), ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/System/Info/Public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json.is_object());
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

#[tokio::test]
async fn items_returns_query_result_of_base_item_dto() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Items?startIndex=0&limit=10&recursive=true")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["TotalRecordCount"], 1);
    assert_eq!(json["StartIndex"], 0);
    assert_eq!(json["Items"][0]["Id"], item_id.to_string());
    assert_eq!(json["Items"][0]["Name"], "Test Item");
}

#[tokio::test]
async fn item_by_id_returns_base_item_dto() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["Id"], item_id.to_string());
    assert_eq!(json["Name"], "Test Item");
}

#[tokio::test]
async fn item_by_id_missing_is_404() {
    // The library knows only `item_id`; a different id resolves to `None` → 404.
    let router = create_router(ok_state(Uuid::from_u128(0xABCD), ""));
    let other = Uuid::from_u128(0xBEEF);
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{other}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn user_views_returns_query_result() {
    let item_id = Uuid::from_u128(0x1111);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/UserViews")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["TotalRecordCount"], 1);
    assert_eq!(json["Items"][0]["Id"], item_id.to_string());
}

#[tokio::test]
async fn playback_info_get_returns_media_sources() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, "/tmp/hermit-test-media.mp4"));
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
        "/tmp/hermit-test-media.mp4"
    );
}

#[tokio::test]
async fn playback_info_post_returns_media_sources() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, "/tmp/hermit-test-media.mp4"));
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

/// Writes a temp file with known bytes and returns its path.
fn write_temp_media(name: &str, contents: &[u8]) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(name);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(contents).unwrap();
    path
}

#[tokio::test]
async fn video_stream_serves_full_body() {
    let item_id = Uuid::from_u128(0xABCD);
    let path = write_temp_media("hermit-stream-full.bin", b"0123456789");
    let router = create_router(ok_state(item_id, path.to_str().unwrap()));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Videos/{item_id}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"0123456789");
}

#[tokio::test]
async fn video_stream_range_request_is_206_with_content_range() {
    let item_id = Uuid::from_u128(0xABCD);
    let path = write_temp_media("hermit-stream-range.bin", b"0123456789");
    let router = create_router(ok_state(item_id, path.to_str().unwrap()));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Videos/{item_id}/stream"))
                .header(header::RANGE, "bytes=2-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let content_range = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(content_range, "bytes 2-5/10");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"2345");
}

#[tokio::test]
async fn item_image_missing_is_404() {
    // No image processor is wired, so a valid item + valid image type still 404s
    // (there is no image path to serve) — the contract's not-found outcome.
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Images/Primary"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn item_image_bad_type_is_400() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Images/NotAnImageType"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// --- Batch 2: item writes/delete + ancestors/counts ---------------------------

/// A [`ProviderManager`] that records the last queued refresh so the
/// `POST /Items/{itemId}/Refresh` handler can be observed end-to-end.
struct RecordingProviders {
    queued: std::sync::Arc<std::sync::Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl hermit_traits::providers::ProviderManager for RecordingProviders {
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        _options: &hermit_traits::providers::MetadataRefreshOptions,
        _priority: hermit_traits::providers::RefreshPriority,
    ) -> Result<(), ServiceError> {
        self.queued.lock().unwrap().push(item_id);
        Ok(())
    }
    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &hermit_traits::providers::MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &hermit_traits::providers::MetadataRefreshOptions,
    ) -> Result<hermit_traits::providers::ItemUpdateType, ServiceError> {
        unimplemented!()
    }
    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: hermit_model::entities::ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: hermit_model::entities::ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &hermit_model::providers::RemoteImageQuery,
    ) -> Result<Vec<hermit_model::providers::RemoteImageInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<hermit_model::providers::ImageProviderInfo>, ServiceError> {
        unimplemented!()
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: hermit_traits::providers::ItemUpdateType,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_external_urls(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<hermit_model::providers::ExternalUrl>, ServiceError> {
        unimplemented!()
    }
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<hermit_model::providers::ExternalIdInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<hermit_model::configuration::MetadataPluginSummary>, ServiceError> {
        unimplemented!()
    }
    async fn get_metadata_options(
        &self,
        _item_id: Uuid,
    ) -> Result<hermit_model::configuration::MetadataOptions, ServiceError> {
        unimplemented!()
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
}

/// Builds an [`AppState`] like [`ok_state`] but with a [`RecordingProviders`] so
/// the refresh handler's queue call is observable.
fn state_with_providers(
    item_id: Uuid,
    queued: std::sync::Arc<std::sync::Mutex<Vec<Uuid>>>,
) -> AppState {
    AppState::new(
        Arc::new(OkLibrary { item_id }),
        Arc::new(OkUsers),
        Arc::new(OkUserViews { item_id }),
        Arc::new(FakeUserData),
        Arc::new(OkMediaSources {
            path: String::new(),
        }),
        Arc::new(OkSessions),
        Arc::new(FakeSystem),
        Arc::new(hermit_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(RecordingProviders { queued }),
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

/// A `GET /Items` request with a wide filter set is accepted (comma/pipe
/// parameters parse) and returns the query result.
#[tokio::test]
async fn get_items_with_filters_returns_query_result() {
    let item_id = Uuid::from_u128(0x51);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri(
                    "/Items?includeItemTypes=Movie,Series&sortBy=SortName&sortOrder=Descending\
                     &filters=IsFavorite&genres=Action|Sci-Fi&years=1999,2001&isFavorite=true",
                )
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["TotalRecordCount"], 1);
    assert_eq!(json["Items"][0]["Id"], item_id.to_string());
}

/// A `GET /Items` with an unknown enum token is a `400`.
#[tokio::test]
async fn get_items_bad_enum_token_is_400() {
    let router = create_router(ok_state(Uuid::from_u128(0x52), ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Items?includeItemTypes=Nonsense")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// `GET /Items/Counts` returns the per-kind counts.
#[tokio::test]
async fn item_counts_returns_counts() {
    let router = create_router(ok_state(Uuid::from_u128(0x53), ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Items/Counts")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["MovieCount"], 3);
    assert_eq!(json["SeriesCount"], 1);
}

/// `GET /Items/{itemId}/Ancestors` returns an array (empty for a root item).
#[tokio::test]
async fn ancestors_of_root_item_is_empty_array() {
    let item_id = Uuid::from_u128(0x54);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Ancestors"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json.as_array().is_some_and(std::vec::Vec::is_empty));
}

/// `GET /Items/{itemId}/Ancestors` for a missing item is a `404`.
#[tokio::test]
async fn ancestors_of_missing_item_is_404() {
    let router = create_router(ok_state(Uuid::from_u128(0x55), ""));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{}/Ancestors", Uuid::from_u128(0xDEAD)))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `DELETE /Items/{itemId}` deletes an existing item (`204`).
#[tokio::test]
async fn delete_item_returns_204() {
    let item_id = Uuid::from_u128(0x56);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// `DELETE /Items/{itemId}` for a missing item is a `404`.
#[tokio::test]
async fn delete_missing_item_is_404() {
    let router = create_router(ok_state(Uuid::from_u128(0x57), ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Items/{}", Uuid::from_u128(0xBEEF)))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `DELETE /Items?ids=...` deletes each listed item (`204`).
#[tokio::test]
async fn delete_items_batch_returns_204() {
    let item_id = Uuid::from_u128(0x58);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Items?ids={item_id}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// `POST /Items/{itemId}` applies an edited item and returns `204`.
#[tokio::test]
async fn update_item_returns_204() {
    let item_id = Uuid::from_u128(0x59);
    let router = create_router(ok_state(item_id, ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video","Name":"Renamed","Genres":["Action","action"],"LockData":true}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// `POST /Items/{itemId}` for a missing item is a `404`.
#[tokio::test]
async fn update_missing_item_is_404() {
    let router = create_router(ok_state(Uuid::from_u128(0x5A), ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{}", Uuid::from_u128(0xF00D)))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"Id":"{}","Type":"Movie","MediaType":"Video","Name":"X"}}"#,
                    Uuid::from_u128(0xF00D)
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `POST /Items/{itemId}/Refresh` queues a refresh for the item (`204`).
#[tokio::test]
async fn refresh_item_queues_and_returns_204() {
    let item_id = Uuid::from_u128(0x5B);
    let queued = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let router = create_router(state_with_providers(item_id, queued.clone()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/Items/{item_id}/Refresh?metadataRefreshMode=FullRefresh"
                ))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(queued.lock().unwrap().as_slice(), &[item_id]);
}

/// `POST /Items/{itemId}/Refresh` for a missing item is a `404` (never queues).
#[tokio::test]
async fn refresh_missing_item_is_404() {
    let queued = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let router = create_router(state_with_providers(Uuid::from_u128(0x5C), queued.clone()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{}/Refresh", Uuid::from_u128(0xC0DE)))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(queued.lock().unwrap().is_empty());
}

/// `POST /Items/{itemId}/ContentType` for a missing item is a `404` (the
/// success path needs a full `ServerConfiguration`, exercised in `hermit-core`).
#[tokio::test]
async fn content_type_missing_item_is_404() {
    let router = create_router(ok_state(Uuid::from_u128(0x5D), ""));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/Items/{}/ContentType?contentType=movies",
                    Uuid::from_u128(0xFEED)
                ))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
