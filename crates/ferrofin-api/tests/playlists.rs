//! Playlists handler tests: CRUD, items, and share management.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! recording `ferrofin-traits` fakes that authenticate and capture the manager
//! calls, asserting the wire status/body and the arguments handed to the
//! [`PlaylistManager`] seam. Managers a handler never touches reuse the
//! `test_support` panic fakes, catching a handler that strays.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeConfig, FakeMediaSources, FakeMusic, FakeProviders, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSystem, FakeUserData, FakeUserViews,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{BaseItemDto, PlaylistDto};
use ferrofin_model::playlists::{PlaylistCreationRequest, PlaylistCreationResult};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::collections::{
    CollectionCreationOptions, CollectionManager, PlaylistAccess, PlaylistAccessLevel,
    PlaylistManager,
};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{AuthorizationInfo, DtoOptions};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
const PLAYLIST_ID: Uuid = Uuid::from_u128(0x91A);
const COLLECTION_ID: Uuid = Uuid::from_u128(0xC01);
const ITEM_A: Uuid = Uuid::from_u128(0xA1);
const ITEM_B: Uuid = Uuid::from_u128(0xB2);

/// Builds a minimal [`UserEntity`] carrying only the id/name fields the handlers
/// read; every other column is a neutral zero value.
fn user_entity(id: Uuid) -> UserEntity {
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
        username: "alice".to_owned(),
    }
}

/// Builds a minimal audio [`BaseItemEntity`] with the given id; every column
/// other than id/name/type is a neutral zero value.
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
        is_movie: false,
        is_repeat: false,
        is_series: false,
        is_virtual_item: false,
        lufs: None,
        media_type: None,
        name: Some("track".to_owned()),
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
        type_: "Audio".to_owned(),
        unrated_type: None,
        width: None,
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID)),
            is_authenticated: true,
            ..AuthorizationInfo::default()
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
            user: Some(user_entity(USER_ID)),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`UserManager`] resolving the fixed authenticated user.
struct OkUsers;

#[async_trait]
impl UserManager for OkUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(|| user_entity(USER_ID)))
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
    async fn rename_user(&self, _u: Uuid, _o: &str, _n: &str) -> Result<(), ServiceError> {
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
        _user: &UserEntity,
        _server_id: Option<String>,
    ) -> Result<ferrofin_model::dto::UserDto, ServiceError> {
        unimplemented!()
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

/// A [`DtoService`] projecting each entity to a bare id/name DTO.
struct OkDto;

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
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|item| BaseItemDto {
                id: Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil()),
                name: item.name.clone(),
                ..BaseItemDto::default()
            })
            .collect())
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!()
    }
}

/// A recorded `add_item_to_playlist` call: `(playlist_id, item_ids, position)`.
type AddCall = (Uuid, Vec<Uuid>, Option<i32>);
/// A recorded `remove_item_from_playlist` call: `(playlist_id, entry_ids)`.
type RemoveCall = (String, Vec<String>);
/// A recorded `move_item` call: `(playlist_id, entry_id, new_index)`.
type MoveCall = (String, String, i32);

/// A recording [`PlaylistManager`] capturing the last mutating call.
///
/// `access` configures what `get_playlist_access` reports for [`PLAYLIST_ID`]
/// (default `Owner`, so success-path tests pass unchanged); `shares` is the
/// canned `get_playlist_shares` result.
struct RecordingPlaylists {
    created: Mutex<Option<PlaylistCreationRequest>>,
    added: Mutex<Option<AddCall>>,
    removed: Mutex<Option<RemoveCall>>,
    moved: Mutex<Option<MoveCall>>,
    access: PlaylistAccess,
    shares: Vec<ferrofin_model::entities_media::PlaylistUserPermissions>,
}

impl Default for RecordingPlaylists {
    fn default() -> Self {
        Self {
            created: Mutex::default(),
            added: Mutex::default(),
            removed: Mutex::default(),
            moved: Mutex::default(),
            access: PlaylistAccess {
                level: PlaylistAccessLevel::Owner,
                open_access: false,
            },
            shares: Vec::new(),
        }
    }
}

#[async_trait]
impl PlaylistManager for RecordingPlaylists {
    async fn get_playlist_access(
        &self,
        playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<PlaylistAccess, ServiceError> {
        if playlist_id == PLAYLIST_ID {
            Ok(self.access)
        } else {
            Err(ServiceError::not_found("playlist"))
        }
    }
    async fn get_playlist_for_user(
        &self,
        playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<BaseItemEntity, ServiceError> {
        if playlist_id == PLAYLIST_ID {
            Ok(item_entity(playlist_id))
        } else {
            Err(ServiceError::not_found("playlist"))
        }
    }
    async fn create_playlist(
        &self,
        request: &PlaylistCreationRequest,
    ) -> Result<PlaylistCreationResult, ServiceError> {
        *self.created.lock().unwrap() = Some(request.clone());
        Ok(PlaylistCreationResult {
            id: PLAYLIST_ID.to_string(),
        })
    }
    async fn update_playlist(
        &self,
        _request: &ferrofin_model::playlists::PlaylistUpdateRequest,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_playlists(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_playlist_items(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![item_entity(ITEM_A), item_entity(ITEM_B)])
    }
    async fn add_user_to_shares(
        &self,
        _request: &ferrofin_model::playlists::PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn remove_user_from_shares(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
        _share: &ferrofin_model::entities_media::PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_playlist_shares(
        &self,
        _playlist_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::PlaylistUserPermissions>, ServiceError> {
        Ok(self.shares.clone())
    }
    async fn add_item_to_playlist(
        &self,
        playlist_id: Uuid,
        item_ids: &[Uuid],
        position: Option<i32>,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        *self.added.lock().unwrap() = Some((playlist_id, item_ids.to_vec(), position));
        Ok(())
    }
    async fn remove_item_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        *self.removed.lock().unwrap() = Some((playlist_id.to_owned(), entry_ids.to_vec()));
        Ok(())
    }
    async fn move_item(
        &self,
        playlist_id: &str,
        entry_id: &str,
        new_index: i32,
        _calling_user_id: Uuid,
    ) -> Result<(), ServiceError> {
        *self.moved.lock().unwrap() =
            Some((playlist_id.to_owned(), entry_id.to_owned(), new_index));
        Ok(())
    }
    async fn remove_playlists(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A recording [`CollectionManager`] capturing the last mutating call.
#[derive(Default)]
struct RecordingCollections {
    created: Mutex<Option<CollectionCreationOptions>>,
    added: Mutex<Option<(Uuid, Vec<Uuid>)>>,
    removed: Mutex<Option<(Uuid, Vec<Uuid>)>>,
}

#[async_trait]
impl CollectionManager for RecordingCollections {
    async fn create_collection(
        &self,
        options: &CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError> {
        *self.created.lock().unwrap() = Some(options.clone());
        Ok(item_entity(COLLECTION_ID))
    }
    async fn add_to_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        *self.added.lock().unwrap() = Some((collection_id, item_ids.to_vec()));
        Ok(())
    }
    async fn remove_from_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        *self.removed.lock().unwrap() = Some((collection_id, item_ids.to_vec()));
        Ok(())
    }
    async fn get_collections_containing_item(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_collections_folder(
        &self,
        _create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
}

/// Assembles an [`AppState`] over the recording playlist/collection fakes.
fn state(playlists: Arc<RecordingPlaylists>, collections: Arc<RecordingCollections>) -> AppState {
    AppState::new(
        Arc::new(ferrofin_api::test_support::FakeLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(ferrofin_api::test_support::FakeQuickConnect),
        playlists,
        collections,
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
                .header("Authorization", "Token abc")
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
#[tokio::test]
async fn create_playlist_from_body_returns_id() {
    let pl = Arc::new(RecordingPlaylists::default());
    let app = state(pl.clone(), Arc::new(RecordingCollections::default()));
    let body = serde_json::json!({ "Name": "Roadtrip", "Ids": [ITEM_A.to_string()] });
    let (status, bytes) = send(
        app,
        "POST",
        "/Playlists",
        Body::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: PlaylistCreationResult = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result.id, PLAYLIST_ID.to_string());
    let created = pl.created.lock().unwrap().clone().expect("recorded");
    assert_eq!(created.name.as_deref(), Some("Roadtrip"));
    assert_eq!(created.item_id_list, vec![ITEM_A]);
    // The caller's id is used when neither query nor body names one.
    assert_eq!(created.user_id, USER_ID);
}

#[tokio::test]
async fn get_playlist_returns_item_ids() {
    let app = state(
        Arc::new(RecordingPlaylists::default()),
        Arc::new(RecordingCollections::default()),
    );
    let (status, bytes) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: PlaylistDto = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(dto.item_ids, vec![ITEM_A, ITEM_B]);
    assert!(dto.shares.is_empty());
}

#[tokio::test]
async fn get_missing_playlist_is_404() {
    let app = state(
        Arc::new(RecordingPlaylists::default()),
        Arc::new(RecordingCollections::default()),
    );
    let missing = Uuid::from_u128(0xDEAD);
    let (status, _) = send(app, "GET", &format!("/Playlists/{missing}"), Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_playlist_items_pages_and_tags_entry_id() {
    let app = state(
        Arc::new(RecordingPlaylists::default()),
        Arc::new(RecordingCollections::default()),
    );
    let (status, bytes) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}/Items?limit=1"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result.total_record_count, 2);
    assert_eq!(result.items.len(), 1);
    assert_eq!(
        result.items[0].playlist_item_id.as_deref(),
        Some(ITEM_A.simple().to_string().as_str())
    );
}

#[tokio::test]
async fn add_items_to_playlist_records_call() {
    let pl = Arc::new(RecordingPlaylists::default());
    let app = state(pl.clone(), Arc::new(RecordingCollections::default()));
    let (status, _) = send(
        app,
        "POST",
        &format!("/Playlists/{PLAYLIST_ID}/Items?ids={ITEM_A},{ITEM_B}&position=2"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (pid, ids, pos) = pl.added.lock().unwrap().clone().expect("recorded");
    assert_eq!(pid, PLAYLIST_ID);
    assert_eq!(ids, vec![ITEM_A, ITEM_B]);
    assert_eq!(pos, Some(2));
}

#[tokio::test]
async fn remove_items_from_playlist_records_call() {
    let pl = Arc::new(RecordingPlaylists::default());
    let app = state(pl.clone(), Arc::new(RecordingCollections::default()));
    let (status, _) = send(
        app,
        "DELETE",
        &format!("/Playlists/{PLAYLIST_ID}/Items?entryIds={ITEM_A}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (pid, entries) = pl.removed.lock().unwrap().clone().expect("recorded");
    assert_eq!(pid, PLAYLIST_ID.to_string());
    assert_eq!(entries, vec![ITEM_A.to_string()]);
}

#[tokio::test]
async fn move_item_records_call() {
    let pl = Arc::new(RecordingPlaylists::default());
    let app = state(pl.clone(), Arc::new(RecordingCollections::default()));
    let (status, _) = send(
        app,
        "POST",
        &format!("/Playlists/{PLAYLIST_ID}/Items/{ITEM_A}/Move/3"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (pid, entry, idx) = pl.moved.lock().unwrap().clone().expect("recorded");
    assert_eq!(pid, PLAYLIST_ID.to_string());
    assert_eq!(entry, ITEM_A.to_string());
    assert_eq!(idx, 3);
}

#[tokio::test]
async fn get_playlist_users_returns_empty_shares() {
    let app = state(
        Arc::new(RecordingPlaylists::default()),
        Arc::new(RecordingCollections::default()),
    );
    let (status, bytes) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}/Users"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let shares: Vec<ferrofin_model::entities_media::PlaylistUserPermissions> =
        serde_json::from_slice(&bytes).unwrap();
    assert!(shares.is_empty());
}

#[tokio::test]
async fn get_playlist_user_self_is_owner_equivalent() {
    let app = state(
        Arc::new(RecordingPlaylists::default()),
        Arc::new(RecordingCollections::default()),
    );
    let (status, bytes) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}/Users/{USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let perm: ferrofin_model::entities_media::PlaylistUserPermissions =
        serde_json::from_slice(&bytes).unwrap();
    assert_eq!(perm.user_id, USER_ID);
    assert!(perm.can_edit);
}
/// A [`RecordingPlaylists`] whose access probe reports the given level/flag.
fn playlists_with_access(level: PlaylistAccessLevel, open_access: bool) -> Arc<RecordingPlaylists> {
    Arc::new(RecordingPlaylists {
        access: PlaylistAccess { level, open_access },
        ..RecordingPlaylists::default()
    })
}

#[tokio::test]
async fn playlist_edit_routes_forbidden_for_read_access() {
    // A read-only caller (non-edit share / open-access) must get 403 from every
    // edit action — update, add items, remove items, move.
    let pl = playlists_with_access(PlaylistAccessLevel::Read, false);
    let app = state(pl.clone(), Arc::new(RecordingCollections::default()));
    let cases = [
        (
            "POST",
            format!("/Playlists/{PLAYLIST_ID}"),
            Body::from("{}"),
        ),
        (
            "POST",
            format!("/Playlists/{PLAYLIST_ID}/Items?ids={ITEM_A}"),
            Body::empty(),
        ),
        (
            "DELETE",
            format!("/Playlists/{PLAYLIST_ID}/Items?entryIds={ITEM_A}"),
            Body::empty(),
        ),
        (
            "POST",
            format!("/Playlists/{PLAYLIST_ID}/Items/{ITEM_A}/Move/0"),
            Body::empty(),
        ),
    ];
    for (method, uri, body) in cases {
        let (status, _) = send(app.clone(), method, &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // Nothing was recorded: the gate fired before the manager call.
    assert!(pl.added.lock().unwrap().is_none());
    assert!(pl.removed.lock().unwrap().is_none());
    assert!(pl.moved.lock().unwrap().is_none());
}

#[tokio::test]
async fn playlist_users_routes_are_owner_only() {
    // Even a CanEdit share is not enough for share management (C# owner-only).
    let pl = playlists_with_access(PlaylistAccessLevel::CanEdit, false);
    let app = state(pl, Arc::new(RecordingCollections::default()));
    let (status, _) = send(
        app.clone(),
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}/Users"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        app,
        "POST",
        &format!("/Playlists/{PLAYLIST_ID}/Users/{USER_ID}"),
        Body::from("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn remove_user_share_missing_is_not_found() {
    // Owner deleting a share that doesn't exist → the C# 404 branch.
    let pl = playlists_with_access(PlaylistAccessLevel::Owner, false);
    let app = state(pl, Arc::new(RecordingCollections::default()));
    let (status, _) = send(
        app,
        "DELETE",
        &format!("/Playlists/{PLAYLIST_ID}/Users/{USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_playlist_user_owner_quirk_returns_caller_permission() {
    // The C# owner early-return reports the *caller's* permission even when the
    // route names a different user.
    let pl = playlists_with_access(PlaylistAccessLevel::Owner, false);
    let app = state(pl, Arc::new(RecordingCollections::default()));
    let other = Uuid::from_u128(0xDEAD);
    let (status, bytes) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}/Users/{other}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let perm: ferrofin_model::entities_media::PlaylistUserPermissions =
        serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        perm.user_id, USER_ID,
        "the caller, not the asked-about user"
    );
    assert!(perm.can_edit);
}

#[tokio::test]
async fn get_playlist_user_unrelated_caller_is_forbidden() {
    // A read-only caller with no CanEdit share asking about a third user → 403.
    let pl = playlists_with_access(PlaylistAccessLevel::Read, false);
    let app = state(pl, Arc::new(RecordingCollections::default()));
    let other = Uuid::from_u128(0xBEEF);
    let (status, _) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}/Users/{other}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn get_playlist_reports_open_access() {
    let pl = playlists_with_access(PlaylistAccessLevel::Read, true);
    let app = state(pl, Arc::new(RecordingCollections::default()));
    let (status, bytes) = send(
        app,
        "GET",
        &format!("/Playlists/{PLAYLIST_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: PlaylistDto = serde_json::from_slice(&bytes).unwrap();
    assert!(dto.open_access, "the DTO carries the real OpenAccess flag");
}
