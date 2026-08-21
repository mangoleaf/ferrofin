//! Batch-5 handler **success-path** tests: Playstate + Session playback
//! reporting.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and record their calls, asserting
//! the success status (and body shape / recorded arguments). A [`RecordingSessions`]
//! captures the last command/capabilities/logout/etc.; a [`RecordingUserData`]
//! captures the mark-played / mark-unplayed calls. Managers a handler never
//! touches reuse the `test_support` panic fakes, catching a stray call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeConfig, FakeDto, FakeMediaSources, FakeMusic, FakeProviders, FakeSearch,
    FakeSimilarItems, FakeSystem, FakeUserViews,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{NameIdPair, SessionInfoDto, UpdateUserItemDataDto, UserItemDataDto};
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, SessionUserInfo,
    TranscodingInfo,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserDataManager, UserManager};
use ferrofin_traits::media_encoding::{HlsStreamManager, HlsStreamRequest, ServedFile};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use ferrofin_traits::stubs::{
    DisabledAttachmentExtractor, PlaybackRequest, SyncPlayManager, SyncPlaySession,
};
use tower::ServiceExt;
use uuid::Uuid;

use ferrofin_model::sync_play::GroupInfoDto;

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
const GUEST_ID: Uuid = Uuid::from_u128(0x9999);
const ITEM_ID: Uuid = Uuid::from_u128(0xBEEF);
const SESSION_ID: &str = "session-current";

/// Builds a minimal [`UserEntity`] with the given id/name; neutral zero fields.
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
    }
}

/// A blank [`UserItemDataDto`] carrying the given item id + played flag.
fn data_dto(item_id: Uuid, played: bool) -> UserItemDataDto {
    UserItemDataDto {
        rating: None,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: 0,
        play_count: i32::from(played),
        is_favorite: false,
        likes: None,
        last_played_date: None,
        played,
        key: item_id.to_string(),
        item_id,
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] authenticating as [`USER_ID`],
/// carrying a token + client/device fields (so session-id resolution runs).
/// `elevated` authenticates as an API key. Off by default: `GET /Auth/Providers` and `GET /Auth/PasswordResetProviders` are,
/// but most routes in this file are not, and over-gating them would break
/// ordinary clients.
struct OkAuth {
    elevated: bool,
}

fn authed_info_as(elevated: bool) -> AuthorizationInfo {
    AuthorizationInfo {
        is_api_key: elevated,
        ..authed_info()
    }
}

fn authed_info() -> AuthorizationInfo {
    AuthorizationInfo {
        device_id: Some("dev-1".to_owned()),
        device: Some("Test Device".to_owned()),
        client: Some("TestClient".to_owned()),
        version: Some("1.0".to_owned()),
        token: Some("token-abc".into()),
        is_api_key: false,
        user: Some(user_entity(USER_ID, "alice")),
        is_authenticated: true,
    }
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(authed_info_as(self.elevated))
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(authed_info_as(self.elevated))
    }
}

/// A [`UserManager`] resolving the caller + guest and canned auth providers.
struct OkUsers;

#[async_trait]
impl UserManager for OkUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        if id == USER_ID {
            Ok(Some(user_entity(USER_ID, "alice")))
        } else if id == GUEST_ID {
            Ok(Some(user_entity(GUEST_ID, "guest")))
        } else {
            Ok(None)
        }
    }
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        Ok(vec![NameIdPair {
            name: Some("Default".to_owned()),
            id: Some("default".to_owned()),
        }])
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        Ok(vec![NameIdPair {
            name: Some("Reset".to_owned()),
            id: Some("reset".to_owned()),
        }])
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
    async fn get_user_by_name(&self, _n: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn rename_user(&self, _u: Uuid, _o: &str, _n: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_user(&self, _u: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn create_user(&self, _n: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!()
    }
    async fn delete_user(&self, _u: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn reset_password(&self, _u: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn change_password(&self, _u: Uuid, _p: &str) -> Result<(), ServiceError> {
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
    async fn update_configuration(
        &self,
        _u: Uuid,
        _c: &ferrofin_model::configuration::UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(
        &self,
        _u: Uuid,
        _p: &ferrofin_model::users::UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_profile_image(&self, _u: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`LibraryManager`] that resolves [`ITEM_ID`] and nothing else.
struct OkLibrary;

#[async_trait]
impl LibraryManager for OkLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| item_entity(ITEM_ID)))
    }
    async fn query_items(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_list(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _q: &InternalItemsQuery,
        _c: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn create_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_people_names(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(&self, _q: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _t: ferrofin_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// Builds a minimal [`BaseItemEntity`] for [`ITEM_ID`] (Movie).
fn item_entity(id: Uuid) -> BaseItemEntity {
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
        is_movie: true,
        is_repeat: false,
        is_series: false,
        is_virtual_item: false,
        lufs: None,
        media_type: None,
        name: Some("Movie".to_owned()),
        normalization_gain: None,
        official_rating: None,
        extra_ids: None,
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
        type_: "Movie".to_owned(),
        unrated_type: None,
        width: None,
    }
}

/// What the [`RecordingSessions`] fake last observed.
#[derive(Default)]
struct SessionCalls {
    played_starts: usize,
    progress: usize,
    stops: usize,
    general_command: Option<GeneralCommand>,
    play: Option<PlayRequest>,
    playstate: Option<PlaystateRequest>,
    message: Option<MessageCommand>,
    capabilities: Option<(String, ClientCapabilities)>,
    added_user: Option<(String, Uuid)>,
    removed_user: Option<(String, Uuid)>,
    now_viewing: Option<(String, String)>,
    logged_out: Option<String>,
}

/// A recording [`SessionManager`]: `log_session_activity` yields a fixed session
/// (optionally carrying an additional guest user), every command records its
/// arguments. Unused methods panic.
struct RecordingSessions {
    calls: Arc<Mutex<SessionCalls>>,
    with_guest: bool,
}

fn current_session_dto(with_guest: bool) -> SessionInfoDto {
    SessionInfoDto {
        id: Some(SESSION_ID.to_owned()),
        user_id: USER_ID,
        additional_users: with_guest.then(|| {
            vec![SessionUserInfo {
                user_id: GUEST_ID,
                user_name: Some("guest".to_owned()),
            }]
        }),
        ..SessionInfoDto::default()
    }
}

#[async_trait]
impl SessionManager for RecordingSessions {
    async fn log_session_activity(
        &self,
        _app_name: &str,
        _app_version: &str,
        _device_id: &str,
        _device_name: &str,
        _remote_endpoint: &str,
        _user: &UserEntity,
    ) -> Result<SessionInfoDto, ServiceError> {
        Ok(current_session_dto(self.with_guest))
    }
    async fn on_playback_start(&self, _i: &PlaybackStartInfo) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().played_starts += 1;
        Ok(())
    }
    async fn on_playback_progress(
        &self,
        _i: &PlaybackProgressInfo,
        _a: bool,
    ) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().progress += 1;
        Ok(())
    }
    async fn on_playback_stopped(&self, _i: &PlaybackStopInfo) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().stops += 1;
        Ok(())
    }
    async fn send_general_command(
        &self,
        _c: &str,
        _s: &str,
        command: &GeneralCommand,
    ) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().general_command = Some(command.clone());
        Ok(())
    }
    async fn send_message_command(
        &self,
        _c: &str,
        _s: &str,
        command: &MessageCommand,
    ) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().message = Some(command.clone());
        Ok(())
    }
    async fn send_play_command(
        &self,
        _c: &str,
        _s: &str,
        command: &PlayRequest,
    ) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().play = Some(command.clone());
        Ok(())
    }
    async fn send_playstate_command(
        &self,
        _c: &str,
        _s: &str,
        command: &PlaystateRequest,
    ) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().playstate = Some(command.clone());
        Ok(())
    }
    async fn add_additional_user(&self, s: &str, u: Uuid) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().added_user = Some((s.to_owned(), u));
        Ok(())
    }
    async fn remove_additional_user(&self, s: &str, u: Uuid) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().removed_user = Some((s.to_owned(), u));
        Ok(())
    }
    async fn report_now_viewing_item(&self, s: &str, i: &str) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().now_viewing = Some((s.to_owned(), i.to_owned()));
        Ok(())
    }
    async fn report_capabilities(
        &self,
        session_id: &str,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().capabilities =
            Some((session_id.to_owned(), capabilities.clone()));
        Ok(())
    }
    async fn get_sessions(
        &self,
        _user_id: Uuid,
        _device_id: Option<&str>,
        _active_within_seconds: Option<i32>,
        _controllable_user_to_check: Option<Uuid>,
        _is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError> {
        Ok(vec![current_session_dto(false)])
    }
    async fn logout(&self, access_token: &str) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().logged_out = Some(access_token.to_owned());
        Ok(())
    }
    async fn update_device_name(&self, _s: &str, _n: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn report_session_ended(&self, _s: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_admin_sessions(
        &self,
        _m: SessionMessageType,
        _d: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_message_to_user_sessions(
        &self,
        _u: &[Uuid],
        _m: SessionMessageType,
        _d: &str,
    ) -> Result<(), ServiceError> {
        // The played/favorite/progress handlers push `UserDataChanged` here;
        // delivery is covered by the ferrofin-core session-manager tests.
        Ok(())
    }
    async fn send_message_to_user_device_sessions(
        &self,
        _d: &str,
        _m: SessionMessageType,
        _p: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn authenticate_new_session(
        &self,
        _r: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        unimplemented!()
    }
    async fn authenticate_direct(
        &self,
        _r: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        unimplemented!()
    }
    async fn report_transcoding_info(
        &self,
        _d: &str,
        _i: &TranscodingInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_transcoding_info(&self, _d: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_session_by_authentication_token(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!()
    }
    async fn logout_device(&self, _d: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn revoke_user_tokens(&self, _u: Uuid, _t: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn close_live_stream_if_needed(&self, _l: &str, _s: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A recording [`UserDataManager`]: mark-played / mark-unplayed record the users
/// they were called for.
#[derive(Default)]
struct RecordingUserData {
    played: Arc<Mutex<Vec<Uuid>>>,
    unplayed: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl UserDataManager for RecordingUserData {
    async fn mark_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        _date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        self.played.lock().unwrap().push(user_id);
        Ok(data_dto(item_id, true))
    }
    async fn mark_unplayed(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        self.unplayed.lock().unwrap().push(user_id);
        Ok(data_dto(item_id, false))
    }
    async fn save_user_data(
        &self,
        _u: Uuid,
        _i: Uuid,
        _d: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_user_data_dto(
        &self,
        _i: Uuid,
        _u: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_data_batch(
        &self,
        _i: &[Uuid],
        _u: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        unimplemented!()
    }
    async fn update_play_state(
        &self,
        _u: Uuid,
        _i: Uuid,
        _p: Option<i64>,
    ) -> Result<bool, ServiceError> {
        unimplemented!()
    }
    async fn reset_playback_stream_selections(
        &self,
        _u: Uuid,
        _i: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A recording [`HlsStreamManager`]: the keep-alive ping and the stop-encoding
/// kill record their arguments; methods the playstate handlers never touch
/// panic, catching a stray call.
#[derive(Default)]
struct RecordingHls {
    /// Every `ping_transcoding_job` call: `(play_session_id, is_user_paused)`.
    pings: Mutex<Vec<(String, Option<bool>)>>,
    /// Every `stop_encoding` call: `(device_id, play_session_id)`.
    stops: Mutex<Vec<(Option<String>, Option<String>)>>,
}

#[async_trait]
impl HlsStreamManager for RecordingHls {
    async fn master_playlist(
        &self,
        _r: &HlsStreamRequest,
        _a: bool,
    ) -> Result<String, ServiceError> {
        unimplemented!()
    }
    async fn variant_playlist(
        &self,
        _r: &HlsStreamRequest,
        _a: bool,
    ) -> Result<String, ServiceError> {
        unimplemented!()
    }
    async fn live_playlist(&self, _r: &HlsStreamRequest) -> Result<String, ServiceError> {
        unimplemented!()
    }
    async fn dynamic_segment(
        &self,
        _r: &HlsStreamRequest,
        _s: i32,
        _a: bool,
    ) -> Result<ServedFile, ServiceError> {
        unimplemented!()
    }
    async fn resolve_transcode_file(&self, _f: &str, _m: bool) -> Result<ServedFile, ServiceError> {
        unimplemented!()
    }
    async fn transcode_stream(
        &self,
        _r: &HlsStreamRequest,
        _a: bool,
    ) -> Result<ServedFile, ServiceError> {
        unimplemented!()
    }
    async fn stop_encoding(&self, request: &HlsStreamRequest) -> Result<(), ServiceError> {
        self.stops
            .lock()
            .unwrap()
            .push((request.device_id.clone(), request.play_session_id.clone()));
        Ok(())
    }
    async fn ping_transcoding_job(
        &self,
        play_session_id: &str,
        is_user_paused: Option<bool>,
    ) -> Result<(), ServiceError> {
        self.pings
            .lock()
            .unwrap()
            .push((play_session_id.to_owned(), is_user_paused));
        Ok(())
    }
}

/// Builds an [`AppState`] with the recording session + user-data fakes.
fn state(sessions: Arc<RecordingSessions>, user_data: Arc<RecordingUserData>) -> AppState {
    state_as(sessions, user_data, false)
}

/// `state` for a caller satisfying `RequiresElevation` — `GET /Auth/Providers`
/// and `GET /Auth/PasswordResetProviders` are admin-only upstream.
fn elevated_state(sessions: Arc<RecordingSessions>, user_data: Arc<RecordingUserData>) -> AppState {
    state_as(sessions, user_data, true)
}

fn state_as(
    sessions: Arc<RecordingSessions>,
    user_data: Arc<RecordingUserData>,
    elevated: bool,
) -> AppState {
    AppState::new(
        Arc::new(OkLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        user_data,
        Arc::new(FakeMediaSources),
        sessions,
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(OkAuth { elevated }),
        Arc::new(OkAuth { elevated }),
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
    )
}

/// Drives one authenticated request through the router, returning (status, body).
async fn send(app: AppState, method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    let response = create_router(app)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", "Token token-abc")
                .header("Content-Type", "application/json")
                .body(body)
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, bytes)
}

/// Like [`state`], but with the recording HLS fake wired in place of the
/// disabled transcode runtime (the attachment seam stays disabled).
fn state_with_hls(
    sessions: Arc<RecordingSessions>,
    user_data: Arc<RecordingUserData>,
    hls: Arc<RecordingHls>,
) -> AppState {
    state(sessions, user_data).with_media_encoding(hls, Arc::new(DisabledAttachmentExtractor))
}

fn recording() -> (Arc<RecordingSessions>, Arc<RecordingUserData>) {
    (
        Arc::new(RecordingSessions {
            calls: Arc::new(Mutex::new(SessionCalls::default())),
            with_guest: false,
        }),
        Arc::new(RecordingUserData::default()),
    )
}

/// A canned [`SyncPlayManager`]: group ops return a fixed group, playback
/// requests succeed. Enough to drive the `/SyncPlay/*` handler wiring.
const GROUP_ID: Uuid = Uuid::from_u128(0xABCD);

struct FakeSyncPlay;

#[async_trait]
impl SyncPlayManager for FakeSyncPlay {
    async fn new_group(
        &self,
        session: &SyncPlaySession,
        group_name: &str,
    ) -> Result<GroupInfoDto, ServiceError> {
        Ok(GroupInfoDto {
            group_id: GROUP_ID,
            group_name: group_name.to_owned(),
            participants: vec![session.user_name.clone()],
            ..Default::default()
        })
    }
    async fn join_group(&self, _s: &SyncPlaySession, _g: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn leave_group(&self, _s: &SyncPlaySession) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn list_groups(&self, _s: &SyncPlaySession) -> Result<Vec<GroupInfoDto>, ServiceError> {
        Ok(vec![GroupInfoDto {
            group_id: GROUP_ID,
            group_name: "g".to_owned(),
            ..Default::default()
        }])
    }
    async fn get_group(
        &self,
        _s: &SyncPlaySession,
        group_id: Uuid,
    ) -> Result<GroupInfoDto, ServiceError> {
        Ok(GroupInfoDto {
            group_id,
            ..Default::default()
        })
    }
    async fn handle_request(
        &self,
        _s: &SyncPlaySession,
        _r: PlaybackRequest,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn is_user_active(&self, _u: Uuid) -> Result<bool, ServiceError> {
        Ok(true)
    }
}

/// An authenticated state with the fake SyncPlay manager wired in.
fn sync_state() -> AppState {
    let (sessions, user_data) = recording();
    state(sessions, user_data).with_sync_play(Arc::new(FakeSyncPlay))
}

#[tokio::test]
async fn sync_play_new_returns_group_with_participant() {
    let (status, body) = send(
        sync_state(),
        "POST",
        "/SyncPlay/New",
        Body::from(r#"{"GroupName":"movie night"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["GroupName"], "movie night");
    // One participant — the handler resolved the caller's session and passed it
    // through to the manager (the fake session's user name is empty).
    assert_eq!(v["Participants"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn sync_play_list_get_and_playback_ops() {
    let (status, body) = send(sync_state(), "GET", "/SyncPlay/List", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);

    let (status, _) = send(
        sync_state(),
        "GET",
        &format!("/SyncPlay/{GROUP_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The body-carrying and bodyless playback ops all resolve to 204.
    let cases: &[(&str, &str)] = &[
        (
            "/SyncPlay/SetNewQueue",
            r#"{"PlayingQueue":[],"PlayingItemPosition":0,"StartPositionTicks":0}"#,
        ),
        ("/SyncPlay/Seek", r#"{"PositionTicks":100}"#),
        ("/SyncPlay/Queue", r#"{"ItemIds":[],"Mode":"Queue"}"#),
        ("/SyncPlay/Pause", "{}"),
        ("/SyncPlay/Unpause", "{}"),
        ("/SyncPlay/Ping", r#"{"Ping":5}"#),
        ("/SyncPlay/SetRepeatMode", r#"{"Mode":"RepeatAll"}"#),
    ];
    for (uri, body) in cases {
        let (status, _) = send(sync_state(), "POST", uri, Body::from(*body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{uri}");
    }

    let (status, _) = send(sync_state(), "POST", "/SyncPlay/Leave", Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn sync_play_returns_501_when_manager_unwired() {
    // The route is registered, but with no SyncPlay manager composed in the
    // handler yields `501` rather than `404`.
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions, user_data),
        "GET",
        "/SyncPlay/List",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

// ── SyncPlay access policy (C# `SyncPlayAccessHandler`) ────────────────────

/// Authenticates as [`USER_ID`] with a chosen stored `SyncPlayAccess` value, so
/// the policy table can be driven from a test.
struct PolicyAuth(i32);

impl PolicyAuth {
    fn info(&self) -> AuthorizationInfo {
        let mut user = user_entity(USER_ID, "alice");
        user.sync_play_access = self.0;
        AuthorizationInfo {
            user: Some(user),
            ..authed_info()
        }
    }
}

#[async_trait]
impl AuthService for PolicyAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(self.info())
    }
}

#[async_trait]
impl AuthorizationContext for PolicyAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(self.info())
    }
}

/// A SyncPlay manager with a fixed `is_user_active`, recording whether any group
/// operation was reached — so a test can prove a denial short-circuits.
struct GatedSyncPlay {
    active: bool,
    reached: Arc<std::sync::atomic::AtomicBool>,
}

impl GatedSyncPlay {
    fn touch(&self) {
        self.reached
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl SyncPlayManager for GatedSyncPlay {
    async fn new_group(
        &self,
        _s: &SyncPlaySession,
        _n: &str,
    ) -> Result<GroupInfoDto, ServiceError> {
        self.touch();
        Ok(GroupInfoDto::default())
    }
    async fn join_group(&self, _s: &SyncPlaySession, _g: Uuid) -> Result<(), ServiceError> {
        self.touch();
        Ok(())
    }
    async fn leave_group(&self, _s: &SyncPlaySession) -> Result<(), ServiceError> {
        self.touch();
        Ok(())
    }
    async fn list_groups(&self, _s: &SyncPlaySession) -> Result<Vec<GroupInfoDto>, ServiceError> {
        self.touch();
        Ok(Vec::new())
    }
    async fn get_group(&self, _s: &SyncPlaySession, g: Uuid) -> Result<GroupInfoDto, ServiceError> {
        self.touch();
        Ok(GroupInfoDto {
            group_id: g,
            ..Default::default()
        })
    }
    async fn handle_request(
        &self,
        _s: &SyncPlaySession,
        _r: PlaybackRequest,
    ) -> Result<(), ServiceError> {
        self.touch();
        Ok(())
    }
    async fn is_user_active(&self, _u: Uuid) -> Result<bool, ServiceError> {
        Ok(self.active)
    }
}

/// A state whose caller has `access` and whose SyncPlay membership is `active`,
/// plus the "the manager was reached" flag.
fn policy_state(access: i32, active: bool) -> (AppState, Arc<std::sync::atomic::AtomicBool>) {
    let (sessions, user_data) = recording();
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let auth = Arc::new(PolicyAuth(access));
    let state = AppState::new(
        Arc::new(OkLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        user_data,
        Arc::new(FakeMediaSources),
        sessions,
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::clone(&auth) as Arc<dyn AuthorizationContext>,
        auth,
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
    )
    .with_sync_play(Arc::new(GatedSyncPlay {
        active,
        reached: Arc::clone(&reached),
    }));
    (state, reached)
}

/// Stored `SyncPlayAccess` discriminants (`Users.SyncPlayAccess`).
const CREATE_AND_JOIN: i32 = 0;
const JOIN_ONLY: i32 = 1;
const NO_SYNC_PLAY: i32 = 2;

#[tokio::test]
async fn sync_play_new_requires_create_access() {
    for (access, expected) in [
        (CREATE_AND_JOIN, StatusCode::OK),
        (JOIN_ONLY, StatusCode::FORBIDDEN),
        (NO_SYNC_PLAY, StatusCode::FORBIDDEN),
    ] {
        let (state, reached) = policy_state(access, false);
        let (status, _) = send(
            state,
            "POST",
            "/SyncPlay/New",
            Body::from(r#"{"GroupName":"g"}"#),
        )
        .await;
        assert_eq!(status, expected, "access {access}");
        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            expected == StatusCode::OK,
            "a denied create must not reach the manager (access {access})"
        );
    }
}

#[tokio::test]
async fn sync_play_join_and_list_require_join_access() {
    for (method, uri, body) in [
        (
            "POST",
            "/SyncPlay/Join",
            r#"{"GroupId":"00000000-0000-0000-0000-000000000001"}"#,
        ),
        ("GET", "/SyncPlay/List", ""),
        ("GET", "/SyncPlay/00000000-0000-0000-0000-000000000001", ""),
    ] {
        for (access, permitted) in [
            (CREATE_AND_JOIN, true),
            (JOIN_ONLY, true),
            (NO_SYNC_PLAY, false),
        ] {
            let (state, reached) = policy_state(access, false);
            let (status, _) = send(state, method, uri, Body::from(body)).await;
            if permitted {
                assert_ne!(status, StatusCode::FORBIDDEN, "{uri} access {access}");
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "{uri} access {access}");
            }
            assert_eq!(
                reached.load(std::sync::atomic::Ordering::SeqCst),
                permitted,
                "{uri} access {access}"
            );
        }
    }
}

#[tokio::test]
async fn sync_play_playback_verbs_require_group_membership() {
    // `IsInGroup` is membership alone — a full-access user who is not in a group
    // is refused, and a user whose policy is `None` but who *is* in a group is
    // allowed (they were downgraded mid-session; C# lets them keep playing).
    for (access, active, expected) in [
        (CREATE_AND_JOIN, true, StatusCode::NO_CONTENT),
        (CREATE_AND_JOIN, false, StatusCode::FORBIDDEN),
        (NO_SYNC_PLAY, true, StatusCode::NO_CONTENT),
        (NO_SYNC_PLAY, false, StatusCode::FORBIDDEN),
    ] {
        let (state, reached) = policy_state(access, active);
        let (status, _) = send(state, "POST", "/SyncPlay/Pause", Body::from("{}")).await;
        assert_eq!(status, expected, "access {access} active {active}");
        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            active,
            "a non-member's playback verb must not reach the manager"
        );
    }
}

#[tokio::test]
async fn sync_play_leave_requires_group_membership() {
    let (state, reached) = policy_state(CREATE_AND_JOIN, false);
    let (status, _) = send(state, "POST", "/SyncPlay/Leave", Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!reached.load(std::sync::atomic::Ordering::SeqCst));

    let (state, reached) = policy_state(CREATE_AND_JOIN, true);
    let (status, _) = send(state, "POST", "/SyncPlay/Leave", Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn mark_played_returns_dto_and_hits_user_data() {
    let (sessions, user_data) = recording();
    let (status, body) = send(
        state(sessions, user_data.clone()),
        "POST",
        &format!("/UserPlayedItems/{ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&body)
    );
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert!(dto.played);
    assert_eq!(user_data.played.lock().unwrap().as_slice(), &[USER_ID]);
}

#[tokio::test]
async fn mark_played_applies_to_additional_users() {
    let sessions = Arc::new(RecordingSessions {
        calls: Arc::new(Mutex::new(SessionCalls::default())),
        with_guest: true,
    });
    let user_data = Arc::new(RecordingUserData::default());
    let (status, _) = send(
        state(sessions, user_data.clone()),
        "POST",
        &format!("/UserPlayedItems/{ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The caller and the guest both get marked.
    assert_eq!(
        user_data.played.lock().unwrap().as_slice(),
        &[USER_ID, GUEST_ID]
    );
}

#[tokio::test]
async fn mark_unplayed_returns_dto() {
    let (sessions, user_data) = recording();
    let (status, body) = send(
        state(sessions, user_data.clone()),
        "DELETE",
        &format!("/UserPlayedItems/{ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert!(!dto.played);
    assert_eq!(user_data.unplayed.lock().unwrap().as_slice(), &[USER_ID]);
}

#[tokio::test]
async fn mark_played_missing_item_is_404() {
    let (sessions, user_data) = recording();
    let missing = Uuid::from_u128(0xDEAD);
    let (status, _) = send(
        state(sessions, user_data),
        "POST",
        &format!("/UserPlayedItems/{missing}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn report_playback_start_is_204() {
    let (sessions, user_data) = recording();
    let info = PlaybackStartInfo {
        item_id: ITEM_ID,
        ..PlaybackStartInfo::default()
    };
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        "/Sessions/Playing",
        Body::from(serde_json::to_vec(&info).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().played_starts, 1);
}

#[tokio::test]
async fn report_playback_progress_is_204() {
    let (sessions, user_data) = recording();
    let info = PlaybackProgressInfo {
        item_id: ITEM_ID,
        ..PlaybackProgressInfo::default()
    };
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        "/Sessions/Playing/Progress",
        Body::from(serde_json::to_vec(&info).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().progress, 1);
}

#[tokio::test]
async fn report_playback_stopped_is_204() {
    let (sessions, user_data) = recording();
    let info = PlaybackStopInfo {
        item_id: ITEM_ID,
        ..PlaybackStopInfo::default()
    };
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        "/Sessions/Playing/Stopped",
        Body::from(serde_json::to_vec(&info).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().stops, 1);
}

#[tokio::test]
async fn ping_playback_session_is_204() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions, user_data),
        "POST",
        "/Sessions/Playing/Ping?playSessionId=ps-1",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn obsolete_playing_items_start_is_204() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/PlayingItems/{ITEM_ID}?playMethod=DirectPlay"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().played_starts, 1);
}

#[tokio::test]
async fn get_sessions_returns_list() {
    let (sessions, user_data) = recording();
    let (status, body) = send(
        state(sessions, user_data),
        "GET",
        "/Sessions",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list: Vec<SessionInfoDto> = serde_json::from_slice(&body).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id.as_deref(), Some(SESSION_ID));
}

#[tokio::test]
async fn play_forwards_item_ids() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/Sessions/{SESSION_ID}/Playing?playCommand=PlayNow&itemIds={ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let play = sessions.calls.lock().unwrap().play.clone().expect("play");
    assert_eq!(play.item_ids, vec![ITEM_ID]);
}

#[tokio::test]
async fn send_playstate_command_parses_path_enum() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/Sessions/{SESSION_ID}/Playing/Pause?seekPositionTicks=5"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let ps = sessions
        .calls
        .lock()
        .unwrap()
        .playstate
        .clone()
        .expect("playstate");
    assert_eq!(ps.seek_position_ticks, Some(5));
}

#[tokio::test]
async fn send_general_command_stamps_controlling_user() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/Sessions/{SESSION_ID}/Command/Mute"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cmd = sessions
        .calls
        .lock()
        .unwrap()
        .general_command
        .clone()
        .expect("cmd");
    assert_eq!(cmd.controlling_user_id, USER_ID);
}

#[tokio::test]
async fn send_full_general_command_is_204() {
    let (sessions, user_data) = recording();
    let command = GeneralCommand {
        name: ferrofin_model::session::GeneralCommandType::SetVolume,
        controlling_user_id: Uuid::nil(),
        arguments: std::collections::HashMap::new(),
    };
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/Sessions/{SESSION_ID}/Command"),
        Body::from(serde_json::to_vec(&command).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cmd = sessions
        .calls
        .lock()
        .unwrap()
        .general_command
        .clone()
        .expect("cmd");
    // The controller overwrites the controlling user with the caller's id.
    assert_eq!(cmd.controlling_user_id, USER_ID);
}

#[tokio::test]
async fn send_message_defaults_header() {
    let (sessions, user_data) = recording();
    let command = MessageCommand {
        header: None,
        text: "hello".to_owned(),
        timeout_ms: None,
    };
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/Sessions/{SESSION_ID}/Message"),
        Body::from(serde_json::to_vec(&command).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let msg = sessions.calls.lock().unwrap().message.clone().expect("msg");
    assert_eq!(msg.header.as_deref(), Some("Message from Server"));
}

#[tokio::test]
async fn add_and_remove_user_are_204() {
    let (sessions, user_data) = recording();
    let app = state(sessions.clone(), user_data);
    let (status, _) = send(
        app.clone(),
        "POST",
        &format!("/Sessions/{SESSION_ID}/User/{GUEST_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        sessions.calls.lock().unwrap().added_user,
        Some((SESSION_ID.to_owned(), GUEST_ID))
    );

    let (status, _) = send(
        app,
        "DELETE",
        &format!("/Sessions/{SESSION_ID}/User/{GUEST_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        sessions.calls.lock().unwrap().removed_user,
        Some((SESSION_ID.to_owned(), GUEST_ID))
    );
}

#[tokio::test]
async fn post_capabilities_parses_csv() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        "/Sessions/Capabilities?id=sess-x&playableMediaTypes=Audio,Video&supportsMediaControl=true",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (id, caps) = sessions
        .calls
        .lock()
        .unwrap()
        .capabilities
        .clone()
        .expect("caps");
    assert_eq!(id, "sess-x");
    assert!(caps.supports_media_control);
    assert_eq!(caps.playable_media_types.len(), 2);
}

#[tokio::test]
async fn report_viewing_uses_current_session_when_absent() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        &format!("/Sessions/Viewing?itemId={ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (session, item) = sessions
        .calls
        .lock()
        .unwrap()
        .now_viewing
        .clone()
        .expect("viewing");
    assert_eq!(session, SESSION_ID);
    assert_eq!(item, ITEM_ID.to_string());
}

#[tokio::test]
async fn logout_uses_caller_token() {
    let (sessions, user_data) = recording();
    let (status, _) = send(
        state(sessions.clone(), user_data),
        "POST",
        "/Sessions/Logout",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        sessions.calls.lock().unwrap().logged_out.as_deref(),
        Some("token-abc")
    );
}

#[tokio::test]
async fn auth_providers_are_listed() {
    let (sessions, user_data) = recording();
    let app = elevated_state(sessions, user_data);
    let (status, body) = send(app.clone(), "GET", "/Auth/Providers", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let providers: Vec<NameIdPair> = serde_json::from_slice(&body).expect("providers");
    assert_eq!(providers.len(), 1);

    let (status, body) = send(app, "GET", "/Auth/PasswordResetProviders", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let providers: Vec<NameIdPair> = serde_json::from_slice(&body).expect("providers");
    assert_eq!(providers[0].id.as_deref(), Some("reset"));
}

// Playback reports drive the transcode lifecycle: a progress report carrying
// a play session pings the job keep-alive (with the paused flag), a stop
// report dispatches the kill scoped to the play session + the caller's device.
// The [`RecordingHls`] fake proves the wiring; the disabled-runtime test
// proves a rejected dispatch is swallowed (best-effort) and playstate still
// records.

#[tokio::test]
async fn progress_with_session_pings_transcode_and_records() {
    let (sessions, user_data) = recording();
    let hls = Arc::new(RecordingHls::default());
    let info = PlaybackProgressInfo {
        item_id: ITEM_ID,
        play_session_id: Some("abc".to_owned()),
        is_paused: true,
        ..PlaybackProgressInfo::default()
    };
    let (status, _) = send(
        state_with_hls(sessions.clone(), user_data, hls.clone()),
        "POST",
        "/Sessions/Playing/Progress",
        Body::from(serde_json::to_vec(&info).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().progress, 1);
    // The keep-alive ping carried the play session and the paused flag.
    assert_eq!(
        hls.pings.lock().unwrap().as_slice(),
        &[("abc".to_owned(), Some(true))]
    );
    assert!(hls.stops.lock().unwrap().is_empty());
}

#[tokio::test]
async fn progress_without_session_does_not_ping_transcode() {
    let (sessions, user_data) = recording();
    let hls = Arc::new(RecordingHls::default());
    let info = PlaybackProgressInfo {
        item_id: ITEM_ID,
        ..PlaybackProgressInfo::default()
    };
    let (status, _) = send(
        state_with_hls(sessions.clone(), user_data, hls.clone()),
        "POST",
        "/Sessions/Playing/Progress",
        Body::from(serde_json::to_vec(&info).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().progress, 1);
    // No play session → nothing to keep alive.
    assert!(hls.pings.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stopped_with_session_kills_transcode_and_records() {
    let (sessions, user_data) = recording();
    let hls = Arc::new(RecordingHls::default());
    let info = PlaybackStopInfo {
        item_id: ITEM_ID,
        play_session_id: Some("abc".to_owned()),
        ..PlaybackStopInfo::default()
    };
    let (status, _) = send(
        state_with_hls(sessions.clone(), user_data, hls.clone()),
        "POST",
        "/Sessions/Playing/Stopped",
        Body::from(serde_json::to_vec(&info).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().stops, 1);
    // The kill is scoped to the play session + the caller's device (from auth).
    assert_eq!(
        hls.stops.lock().unwrap().as_slice(),
        &[(Some("dev-1".to_owned()), Some("abc".to_owned()))]
    );
    assert!(hls.pings.lock().unwrap().is_empty());
}

#[tokio::test]
async fn transcode_dispatch_rejection_is_swallowed() {
    // The default state's disabled HLS runtime rejects both the ping and the
    // kill; the handlers must swallow that and still record playstate.
    let (sessions, user_data) = recording();
    let app = state(sessions.clone(), user_data);
    let progress = PlaybackProgressInfo {
        item_id: ITEM_ID,
        play_session_id: Some("ps-1".to_owned()),
        is_paused: true,
        ..PlaybackProgressInfo::default()
    };
    let (status, _) = send(
        app.clone(),
        "POST",
        "/Sessions/Playing/Progress",
        Body::from(serde_json::to_vec(&progress).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().progress, 1);

    let stop = PlaybackStopInfo {
        item_id: ITEM_ID,
        play_session_id: Some("ps-1".to_owned()),
        ..PlaybackStopInfo::default()
    };
    let (status, _) = send(
        app,
        "POST",
        "/Sessions/Playing/Stopped",
        Body::from(serde_json::to_vec(&stop).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().stops, 1);
}

// The path-scoped `/Users/{userId}/PlayingItems/…` aliases forward to the
// obsolete query forms (the path user is ignored; the session comes from auth).

#[tokio::test]
async fn user_scoped_playing_items_start_progress_stop_forward() {
    let user = uuid::Uuid::from_u128(0x11);
    let (sessions, user_data) = recording();
    let hls = Arc::new(RecordingHls::default());
    let (status, _) = send(
        state_with_hls(sessions.clone(), user_data.clone(), hls.clone()),
        "POST",
        &format!("/Users/{user}/PlayingItems/{ITEM_ID}?playMethod=DirectPlay"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().played_starts, 1);

    let (status, _) = send(
        state_with_hls(sessions.clone(), user_data.clone(), hls.clone()),
        "POST",
        &format!(
            "/Users/{user}/PlayingItems/{ITEM_ID}/Progress?playSessionId=ps-1&positionTicks=7"
        ),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().progress, 1);
    // Forwarding reached the transcode keep-alive too (isPaused defaults false).
    assert_eq!(
        hls.pings.lock().unwrap().as_slice(),
        &[("ps-1".to_owned(), Some(false))]
    );

    let (status, _) = send(
        state_with_hls(sessions.clone(), user_data, hls.clone()),
        "DELETE",
        &format!("/Users/{user}/PlayingItems/{ITEM_ID}?playSessionId=ps-1&positionTicks=9"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(sessions.calls.lock().unwrap().stops, 1);
    // The legacy stop form dispatches the transcode kill as well.
    assert_eq!(
        hls.stops.lock().unwrap().as_slice(),
        &[(Some("dev-1".to_owned()), Some("ps-1".to_owned()))]
    );
}
