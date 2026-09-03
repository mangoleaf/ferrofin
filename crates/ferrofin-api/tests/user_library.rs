//! User-library integration tests: user data, favorites, rating, resume,
//! user-scoped browse, user views, grouping options, and media folders.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned data, asserting
//! the success status and the wire-body shape. A tiny in-memory
//! [`RecordingUserData`] captures the last saved [`UpdateUserItemDataDto`] so the
//! favourite/rating writes can be verified. Managers a given handler never
//! touches reuse the `test_support` panic fakes, catching a handler that strays.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeConfig, FakeMediaSources, FakeMusic, FakeProviders, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSystem,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{
    BaseItemDto, SpecialViewOptionDto, UpdateUserItemDataDto, UserItemDataDto,
};
use ferrofin_model::dto::{MediaSourceInfo, SessionInfoDto};
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::media_info::LiveStreamRequest;
use ferrofin_model::querying::QueryResult;
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::MediaSourceManager;
use ferrofin_traits::library::{LibraryManager, UserDataManager, UserManager, UserViewManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
const ITEM_ID: Uuid = Uuid::from_u128(0xBEEF);
const ROOT_ID: Uuid = Uuid::from_u128(0x0F00);
const TRAILER_ID: Uuid = Uuid::from_u128(0xA1);
const SPECIAL_ID: Uuid = Uuid::from_u128(0xA2);
const SHOWS_ID: Uuid = Uuid::from_u128(0x101);
const MOVIES_ID: Uuid = Uuid::from_u128(0x102);
const MUSIC_ID: Uuid = Uuid::from_u128(0x103);
const MIXED_ID: Uuid = Uuid::from_u128(0x104);
const PLAYLISTS_VIEW_ID: Uuid = Uuid::from_u128(0x105);

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

/// Builds a minimal [`BaseItemEntity`] with the given id + name + kind.
fn item_entity(id: Uuid, name: &str, kind: BaseItemKind) -> BaseItemEntity {
    let type_ = serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(std::string::ToString::to_string))
        .unwrap_or_else(|| "Folder".to_owned());
    BaseItemEntity {
        id: id.to_string(),
        album: None,
        album_artists: None,
        artists: None,
        audio: None,
        channel_id: None,
        clean_name: Some(name.to_lowercase()),
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
        name: Some(name.to_owned()),
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
        type_,
        unrated_type: None,
        width: None,
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
/// `elevated` authenticates as an API key. Off by default: `GET /Library/MediaFolders` is `RequiresElevation` upstream,
/// but most routes in this file are not, and over-gating them would break
/// ordinary clients.
struct OkAuth {
    elevated: bool,
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            is_api_key: self.elevated,
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
            user: Some(user_entity(USER_ID, "alice")),
            is_api_key: self.elevated,
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`UserManager`] resolving the fixed authenticated user. The
/// `latest_item_excludes` ride on the user DTO's configuration, where
/// `/Items/Latest` reads them (C# `PreferenceKind.LatestItemExcludes`).
#[derive(Default)]
struct OkUsers {
    latest_item_excludes: Vec<Uuid>,
}

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
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<ferrofin_model::dto::UserDto, ServiceError> {
        Ok(ferrofin_model::dto::UserDto {
            id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
            name: Some(user.username.clone()),
            server_id,
            configuration: Some(ferrofin_model::configuration::UserConfiguration {
                latest_items_excludes: self.latest_item_excludes.clone(),
                ..ferrofin_model::configuration::UserConfiguration::default()
            }),
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

/// A [`LibraryManager`] that resolves [`ITEM_ID`], a [`ROOT_ID`] root folder, and
/// returns trailer/special extras from `get_item_list`.
struct StubLibrary;

#[async_trait]
impl LibraryManager for StubLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)))
    }
    async fn get_user_root_folder(&self) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(Some(item_entity(
            ROOT_ID,
            "Media Folders",
            BaseItemKind::UserRootFolder,
        )))
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        // The resume query surfaces one in-progress item.
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
        ))
    }
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Trailer vs special-feature extras are distinguished by extra_types.
        use ferrofin_model::entities::ExtraType;
        if query.extra_types.contains(&ExtraType::Trailer) {
            Ok(vec![item_entity(
                TRAILER_ID,
                "Trailer",
                BaseItemKind::Trailer,
            )])
        } else if query.extra_types.contains(&ExtraType::BehindTheScenes) {
            Ok(vec![item_entity(SPECIAL_ID, "BTS", BaseItemKind::Video)])
        } else {
            Ok(Vec::new())
        }
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
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
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
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
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
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
        _s: ferrofin_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`UserViewManager`] returning two collection folders as the user's views,
/// plus one latest row for the user-scoped-latest forwarding test.
struct StubUserViews;

#[async_trait]
impl UserViewManager for StubUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![
            item_entity(SHOWS_ID, "Shows", BaseItemKind::CollectionFolder),
            item_entity(MOVIES_ID, "Movies", BaseItemKind::CollectionFolder),
            item_entity(MUSIC_ID, "Music", BaseItemKind::CollectionFolder),
            item_entity(MIXED_ID, "Attic", BaseItemKind::CollectionFolder),
            // A virtual view: no library behind it, so no virtual-folder entry.
            item_entity(PLAYLISTS_VIEW_ID, "Playlists", BaseItemKind::UserView),
        ])
    }
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(user_id).await
    }
    async fn get_latest_items(
        &self,
        _query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        Ok(vec![(
            None,
            vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
        )])
    }
}

/// A [`UserDataManager`] recording the last write and returning a canned DTO.
#[derive(Default)]
struct RecordingUserData {
    last: Mutex<Option<UpdateUserItemDataDto>>,
}

fn canned_dto(item_id: Uuid, favorite: bool, likes: Option<bool>) -> UserItemDataDto {
    UserItemDataDto {
        rating: None,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: 0,
        play_count: 0,
        is_favorite: favorite,
        likes,
        last_played_date: None,
        played: false,
        key: item_id.to_string(),
        item_id,
    }
}

#[async_trait]
impl UserDataManager for RecordingUserData {
    async fn save_user_data(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        *self.last.lock().expect("lock") = Some(user_data.clone());
        Ok(())
    }
    async fn get_user_data_dto(
        &self,
        item_id: Uuid,
        _user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        let last = self.last.lock().expect("lock").clone().unwrap_or_default();
        Ok(Some(canned_dto(
            item_id,
            last.is_favorite.unwrap_or(false),
            last.likes,
        )))
    }
    async fn get_user_data_batch(
        &self,
        _item_ids: &[Uuid],
        _user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        unimplemented!()
    }
    async fn update_play_state(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        unimplemented!()
    }
    async fn mark_played(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        unimplemented!()
    }
    async fn mark_unplayed(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        unimplemented!()
    }
    async fn reset_playback_stream_selections(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<(), ServiceError> {
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
    /// Mirrors the real service on the ONE axis these tests assert: a `user`
    /// produces a `UserData` block, no user produces none. Ignoring the
    /// argument made "media folders carry no UserData" pass whether or not the
    /// handler passed a user.
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|e| {
                let mut dto = entity_to_dto(e);
                if user.is_some() {
                    dto.user_data = Some(canned_dto(Uuid::nil(), false, None));
                }
                dto
            })
            .collect())
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

/// The configured libraries behind the stub views: Shows is `tvshows`, Movies
/// `movies`, Music `music` (grouping-ineligible), Attic `mixed` (untyped → eligible).
fn virtual_folders() -> ferrofin_api::test_support::FakeVirtualFolders {
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_model::entities_media::VirtualFolderInfo;
    let folder = |id: Uuid, name: &str, ct: CollectionTypeOptions| VirtualFolderInfo {
        name: Some(name.to_owned()),
        item_id: Some(id.simple().to_string()),
        collection_type: Some(ct),
        ..VirtualFolderInfo::default()
    };
    ferrofin_api::test_support::FakeVirtualFolders::seeded(vec![
        folder(SHOWS_ID, "Shows", CollectionTypeOptions::tvshows),
        folder(MOVIES_ID, "Movies", CollectionTypeOptions::movies),
        folder(MUSIC_ID, "Music", CollectionTypeOptions::music),
        folder(MIXED_ID, "Attic", CollectionTypeOptions::mixed),
    ])
}

/// Builds an [`AppState`] wired with the user-library stubs.
fn state_as(elevated: bool) -> AppState {
    AppState::new(
        Arc::new(StubLibrary),
        Arc::new(OkUsers::default()),
        Arc::new(StubUserViews),
        Arc::new(RecordingUserData::default()),
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
    .with_virtual_folders(Arc::new(virtual_folders()))
}

/// Drives one request through the router and returns (status, body bytes).
async fn send(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    send_as(method, uri, body, false).await
}

/// [`send`] for a caller satisfying `RequiresElevation`.
async fn send_elevated(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    send_as(method, uri, body, true).await
}

async fn send_as(method: &str, uri: &str, body: Body, elevated: bool) -> (StatusCode, Vec<u8>) {
    let router = create_router(state_as(elevated));
    let response = router
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
async fn mark_favorite_saves_and_returns_dto() {
    let (status, body) = send(
        "POST",
        &format!("/UserFavoriteItems/{ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert!(dto.is_favorite);
    assert_eq!(dto.item_id, ITEM_ID);
}

#[tokio::test]
async fn unmark_favorite_saves_false() {
    let (status, body) = send(
        "DELETE",
        &format!("/UserFavoriteItems/{ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert!(!dto.is_favorite);
}

#[tokio::test]
async fn update_rating_likes_true() {
    let (status, body) = send(
        "POST",
        &format!("/UserItems/{ITEM_ID}/Rating?likes=true"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert_eq!(dto.likes, Some(true));
}

#[tokio::test]
async fn get_and_update_user_data() {
    // GET resolves the item + returns a DTO.
    let (status, body) = send(
        "GET",
        &format!("/UserItems/{ITEM_ID}/UserData"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");

    // POST with a body writes then returns the DTO.
    let payload = serde_json::to_vec(&UpdateUserItemDataDto {
        is_favorite: Some(true),
        ..UpdateUserItemDataDto::default()
    })
    .expect("payload");
    let (status, body) = send(
        "POST",
        &format!("/UserItems/{ITEM_ID}/UserData"),
        Body::from(payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert!(dto.is_favorite);
}

#[tokio::test]
async fn user_data_missing_item_is_404() {
    let missing = Uuid::from_u128(0xDEAD);
    let (status, _) = send(
        "GET",
        &format!("/UserItems/{missing}/UserData"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_returns_in_progress_items() {
    let (status, body) = send("GET", "/UserItems/Resume", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].id, ITEM_ID);
}

// The path-scoped `/Users/{userId}/Items/…` forms jellyfin-web's apiclient calls
// forward to the query-scoped handlers (home rows + the metadata editor).
#[tokio::test]
async fn user_scoped_latest_forwards_to_latest() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/Latest"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dtos: Vec<BaseItemDto> = serde_json::from_slice(&body).expect("dtos");
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, ITEM_ID);
}

#[tokio::test]
async fn user_scoped_resume_forwards_to_resume() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/Resume"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert_eq!(result.items.len(), 1);
}

#[tokio::test]
async fn user_scoped_item_forwards_to_get_item() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/{ITEM_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: BaseItemDto = serde_json::from_slice(&body).expect("dto");
    assert_eq!(dto.id, ITEM_ID);
}

#[tokio::test]
async fn routes_require_auth() {
    // The default `fake_state` uses the rejecting `FakeAuthService`, so a
    // protected user-library route returns `401` (route exists, auth fails)
    // rather than the `501` stub or a `404`.
    let router = create_router(ferrofin_api::test_support::fake_state());
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/UserFavoriteItems/{ITEM_ID}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn user_views_returns_query_result() {
    let (status, body) = send("GET", "/UserViews", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert_eq!(result.items.len(), 5);
    assert_eq!(result.items[0].id, SHOWS_ID);
}

#[tokio::test]
async fn grouping_options_returns_name_sorted_eligible_views() {
    let (status, body) = send("GET", "/UserViews/GroupingOptions", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let opts: Vec<SpecialViewOptionDto> = serde_json::from_slice(&body).expect("options");
    // `UserView.IsEligibleForGrouping`: movies, tvshows and an UNTYPED (mixed) library;
    // the music library and the view with no library behind it are out.
    let names: Vec<&str> = opts.iter().filter_map(|o| o.name.as_deref()).collect();
    assert_eq!(names, ["Attic", "Movies", "Shows"]);
    // Ids are dashless guids.
    assert!(opts[0].id.as_deref().is_some_and(|i| !i.contains('-')));
}

#[tokio::test]
async fn grouping_options_fails_loudly_when_library_types_are_unreadable() {
    // `/UserViews` only decorates with the type (swallowed); for GroupingOptions the
    // type IS the answer, so an unreadable library configuration is an error, never
    // a silently unfiltered or empty list.
    let state = state_as(false).with_virtual_folders(Arc::new(
        ferrofin_api::test_support::FakeVirtualFolders::failing(),
    ));
    let router = create_router(state);
    let mut statuses = Vec::new();
    for uri in ["/UserViews/GroupingOptions", "/UserViews"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("Authorization", "Token abc")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        statuses.push(response.status());
    }
    assert_eq!(
        statuses,
        [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::OK]
    );
}

#[tokio::test]
async fn media_folders_returns_collection_folders() {
    // `GET /Library/MediaFolders` is `RequiresElevation` upstream.
    let (status, body) = send_elevated("GET", "/Library/MediaFolders", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("folders");
    assert_eq!(result.items.len(), 5);

    // No `UserData`, because upstream projects these WITHOUT a user:
    //   var dtoOptions = new DtoOptions().AddClientFields(User);
    //   var resultArray = _dtoService.GetBaseItemDtos(items, dtoOptions);
    // and `IDtoService.GetBaseItemDtos(items, options, User? user = null, …)`
    // defaults the user to null. Passing one added a block Jellyfin never
    // sends AND paid for the user-data prefetch to build it — the endpoint
    // scored `comparable: false` in the perf suite for exactly this.
    let raw: serde_json::Value = serde_json::from_slice(&body).expect("folders json");
    for item in raw["Items"].as_array().expect("Items array") {
        assert!(
            item.get("UserData").is_none(),
            "media folders must carry no UserData — upstream projects them \
             with no user: {item}"
        );
    }
}

// The remaining path-scoped `/Users/{userId}/…` aliases forward to the modern
// handlers with the path user folded in; each must execute the real handler
// body (legacy_routes.rs only proves registration via 401s).

#[tokio::test]
async fn user_scoped_root_forwards_to_items_root() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/Root"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: BaseItemDto = serde_json::from_slice(&body).expect("dto");
    assert_eq!(dto.id, ROOT_ID);
    // The alias must be indistinguishable from the modern query form on the wire.
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/Items/Root?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);
}

#[tokio::test]
async fn user_scoped_intros_returns_empty_query_result() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/{ITEM_ID}/Intros"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert!(result.items.is_empty());
    // The alias must be indistinguishable from the modern query form on the wire.
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/Intros?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);
    // A missing item is still a 404 through the alias.
    let missing = Uuid::from_u128(0xDEAD);
    let (status, _) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/{missing}/Intros"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn user_scoped_local_trailers_and_special_features_forward() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/{ITEM_ID}/LocalTrailers"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dtos: Vec<BaseItemDto> = serde_json::from_slice(&body).expect("dtos");
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, TRAILER_ID);
    // The alias must be indistinguishable from the modern query form on the wire.
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/LocalTrailers?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);

    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/{ITEM_ID}/SpecialFeatures"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dtos: Vec<BaseItemDto> = serde_json::from_slice(&body).expect("dtos");
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, SPECIAL_ID);
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/SpecialFeatures?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);
}

#[tokio::test]
async fn user_scoped_user_data_get_and_post_forward() {
    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/Items/{ITEM_ID}/UserData"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    // The alias must be indistinguishable from the modern query form on the
    // wire (each `send` builds a fresh state, so both reads see pristine data).
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/UserItems/{ITEM_ID}/UserData?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);

    let payload = serde_json::to_vec(&UpdateUserItemDataDto {
        is_favorite: Some(true),
        ..UpdateUserItemDataDto::default()
    })
    .expect("payload");
    let (status, body) = send(
        "POST",
        &format!("/Users/{USER_ID}/Items/{ITEM_ID}/UserData"),
        Body::from(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dto: UserItemDataDto = serde_json::from_slice(&body).expect("dto");
    assert!(dto.is_favorite);
    // Same payload through the modern route yields the same wire response.
    let (modern_status, modern_body) = send(
        "POST",
        &format!("/UserItems/{ITEM_ID}/UserData?userId={USER_ID}"),
        Body::from(payload),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);
}

#[tokio::test]
async fn user_scoped_views_and_grouping_options_forward() {
    let (status, body) = send("GET", &format!("/Users/{USER_ID}/Views"), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert_eq!(result.items.len(), 5);
    // The alias yields the exact body of the modern query-scoped route.
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/UserViews?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);

    let (status, body) = send(
        "GET",
        &format!("/Users/{USER_ID}/GroupingOptions"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let opts: Vec<SpecialViewOptionDto> = serde_json::from_slice(&body).expect("options");
    assert_eq!(opts.len(), 3);
    let (modern_status, modern_body) = send(
        "GET",
        &format!("/UserViews/GroupingOptions?userId={USER_ID}"),
        Body::empty(),
    )
    .await;
    assert_eq!(modern_status, StatusCode::OK);
    assert_eq!(body, modern_body);
}

// ---- `GET /Items/Latest` — the `GetLatestMedia` projection ---------------------
//
// The manager returns the C# `(container, items)` tuples; these tests pin what
// the handler does with them: which entity is emitted, its `ChildCount`, and
// which query/options reach the manager.

const SERIES_ID: Uuid = Uuid::from_u128(0x5E51);
const ALBUM_ID: Uuid = Uuid::from_u128(0xA1B0);
const EP1_ID: Uuid = Uuid::from_u128(0xE001);
const EP2_ID: Uuid = Uuid::from_u128(0xE002);
const TRACK_ID: Uuid = Uuid::from_u128(0x7A01);

/// A [`UserViewManager`] serving canned latest groups and recording the query
/// the handler sent.
struct LatestViews {
    groups: Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>,
    recorded: Mutex<Option<ferrofin_traits::options::LatestItemsQuery>>,
}

impl LatestViews {
    fn new(groups: Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>) -> Arc<Self> {
        Arc::new(Self {
            groups,
            recorded: Mutex::new(None),
        })
    }
}

#[async_trait]
impl UserViewManager for LatestViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("latest fake")
    }
    async fn get_media_folders(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("latest fake")
    }
    async fn get_latest_items(
        &self,
        query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        *self.recorded.lock().expect("lock") = Some(query.clone());
        Ok(self.groups.clone())
    }
}

/// A [`DtoService`] that, like the real one, emits `ImageTags` only when the
/// options enable images — so `enableImages=false` is observable on the wire.
struct LatestDto;

#[async_trait]
impl DtoService for LatestDto {
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
        options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|e| {
                let mut dto = entity_to_dto(e);
                if options.enable_images {
                    dto.image_tags = Some(
                        [(
                            ferrofin_model::entities::ImageType::Primary,
                            "tag".to_owned(),
                        )]
                        .into_iter()
                        .collect(),
                    );
                }
                dto
            })
            .collect())
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

/// An [`AuthService`] / [`AuthorizationContext`] whose user hides played
/// items from "latest" (`HidePlayedInLatest`).
struct HidingAuth;

impl HidingAuth {
    fn info() -> AuthorizationInfo {
        let mut user = user_entity(USER_ID, "alice");
        user.hide_played_in_latest = true;
        AuthorizationInfo {
            user: Some(user),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        }
    }
}

#[async_trait]
impl AuthService for HidingAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(Self::info())
    }
}

#[async_trait]
impl AuthorizationContext for HidingAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(Self::info())
    }
}

/// The user-library state with the latest fakes swapped in.
fn latest_state(
    views: Arc<LatestViews>,
    hide_played: bool,
    latest_item_excludes: Vec<Uuid>,
) -> AppState {
    let (auth_context, auth_service): (Arc<dyn AuthorizationContext>, Arc<dyn AuthService>) =
        if hide_played {
            (Arc::new(HidingAuth), Arc::new(HidingAuth))
        } else {
            (
                Arc::new(OkAuth { elevated: false }),
                Arc::new(OkAuth { elevated: false }),
            )
        };
    AppState::new(
        Arc::new(StubLibrary),
        Arc::new(OkUsers {
            latest_item_excludes,
        }),
        views,
        Arc::new(RecordingUserData::default()),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(LatestDto),
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
        Arc::new(ferrofin_api::test_support::FakeDevices),
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
    .with_virtual_folders(Arc::new(virtual_folders()))
}

/// Drives `GET <uri>` through a latest-wired router; returns the status, the
/// raw body and the parsed DTOs.
async fn send_latest(
    views: &Arc<LatestViews>,
    uri: &str,
    hide_played: bool,
) -> (StatusCode, Vec<u8>, Vec<BaseItemDto>) {
    let router = create_router(latest_state(Arc::clone(views), hide_played, Vec::new()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("Authorization", "Token abc")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    let dtos: Vec<BaseItemDto> = if status == StatusCode::OK {
        serde_json::from_slice(&bytes).expect("dtos")
    } else {
        Vec::new()
    };
    (status, bytes, dtos)
}

fn series() -> BaseItemEntity {
    item_entity(SERIES_ID, "Series", BaseItemKind::Series)
}
fn episode(id: Uuid) -> BaseItemEntity {
    item_entity(id, "Episode", BaseItemKind::Episode)
}
fn album() -> BaseItemEntity {
    item_entity(ALBUM_ID, "Album", BaseItemKind::MusicAlbum)
}
fn track() -> BaseItemEntity {
    item_entity(TRACK_ID, "Track", BaseItemKind::Audio)
}

/// `i.Item1 is not null && i.Item2.Count > 1` → the container, with
/// `ChildCount` = the number of new items under it.
#[tokio::test]
async fn latest_collapses_series_with_two_episodes_into_series_with_child_count() {
    let views = LatestViews::new(vec![(
        Some(series()),
        vec![episode(EP1_ID), episode(EP2_ID)],
    )]);
    let (status, _, dtos) = send_latest(&views, "/Items/Latest", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, SERIES_ID);
    assert_eq!(dtos[0].child_count, Some(2));
}

/// One new episode of a series is the episode itself (`Item2[0]`), not the
/// series — and its `ChildCount` is left alone: v12 restamps only
/// `if (childCounts[i] > 0)` (UserLibraryController.cs:592-598).
#[tokio::test]
async fn latest_returns_single_episode_itself_without_child_count() {
    let views = LatestViews::new(vec![(Some(series()), vec![episode(EP1_ID)])]);
    let (status, _, dtos) = send_latest(&views, "/Items/Latest", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, EP1_ID);
    assert_eq!(dtos[0].child_count, None);
}

/// `|| i.Item1 is MusicAlbum`: an album collapses even with one new track.
#[tokio::test]
async fn latest_always_collapses_music_album_even_with_one_track() {
    let views = LatestViews::new(vec![(Some(album()), vec![track()])]);
    let (status, _, dtos) = send_latest(&views, "/Items/Latest", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, ALBUM_ID);
    assert_eq!(dtos[0].child_count, Some(1));
}

/// An ungrouped row carries no `ChildCount` on the wire: 10.11.8 stamped
/// `dto.ChildCount = childCount` (a serialized `0`) on every row, v12 only
/// `if (childCounts[i] > 0)` (UserLibraryController.cs:592-598), so a movie
/// row's key is absent.
#[tokio::test]
async fn latest_ungrouped_items_carry_no_child_count() {
    let views = LatestViews::new(vec![(
        None,
        vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
    )]);
    let (status, body, dtos) = send_latest(&views, "/Items/Latest", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dtos[0].id, ITEM_ID);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert!(json[0].get("ChildCount").is_none(), "{}", json[0]);
}

/// The shape v12's **music** branch produces: the manager returns the album
/// ROW (not its tracks), so the tuple is `(null, [album])` and the album falls
/// through the collapse test — `Item1 is MusicAlbum` never fires on a null
/// `Item1` (UserLibraryController.cs:580). The album is therefore emitted as
/// itself with `childCount` 0, and `if (childCounts[i] > 0)` (`:592-598`)
/// leaves the key OFF the wire.
///
/// This is the one wire difference the branch makes: the same album used to
/// arrive as a container carrying `ChildCount` = its new-track count.
#[tokio::test]
async fn latest_music_album_row_is_itself_and_carries_no_child_count() {
    let views = LatestViews::new(vec![(None, vec![album()])]);
    let (status, body, dtos) = send_latest(&views, "/Items/Latest", false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, ALBUM_ID);
    // The `MusicAlbum` collapse arm is guarded by `Item1 is not null`, so a
    // null container skips it and no count is stamped. (The stub DTO service
    // does not carry `Type` through, hence the assertion on the id.)
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert!(json[0].get("ChildCount").is_none(), "{}", json[0]);
}

/// The request's `groupItems`/`isPlayed`/`limit`/`parentId`/`includeItemTypes`
/// reach the manager, the user rides along, and a `HidePlayedInLatest` user
/// turns an unset `isPlayed` into `false`.
#[tokio::test]
async fn latest_passes_group_items_and_is_played_through() {
    let views = LatestViews::new(Vec::new());
    let (status, _, _) = send_latest(
        &views,
        &format!("/Items/Latest?groupItems=false&isPlayed=true&limit=7&parentId={SERIES_ID}&includeItemTypes=Episode,Movie"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let q = views
        .recorded
        .lock()
        .expect("lock")
        .clone()
        .expect("the manager was called");
    assert!(!q.group_items);
    assert_eq!(q.is_played, Some(true));
    assert_eq!(q.limit, Some(7));
    assert_eq!(q.parent_id, Some(SERIES_ID));
    assert_eq!(
        q.include_item_types,
        vec![BaseItemKind::Episode, BaseItemKind::Movie]
    );
    assert_eq!(
        q.user.as_ref().map(|u| u.id.as_str()),
        Some(USER_ID.to_string().as_str())
    );

    // Defaults: grouped, limit 20, no played filter for a user who shows
    // played items …
    let (status, _, _) = send_latest(&views, "/Items/Latest", false).await;
    assert_eq!(status, StatusCode::OK);
    let q = views
        .recorded
        .lock()
        .expect("lock")
        .clone()
        .expect("called");
    assert!(q.group_items);
    assert_eq!(q.limit, Some(20));
    assert_eq!(q.is_played, None);

    // … and `isPlayed = false` for one who hides them.
    let (status, _, _) = send_latest(&views, "/Items/Latest", true).await;
    assert_eq!(status, StatusCode::OK);
    let q = views
        .recorded
        .lock()
        .expect("lock")
        .clone()
        .expect("called");
    assert_eq!(q.is_played, Some(false));
}

/// `enableImages=false` reaches the DTO options (C# `AddAdditionalDtoOptions`).
#[tokio::test]
async fn latest_honours_enable_images_false() {
    let views = LatestViews::new(vec![(
        None,
        vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
    )]);
    let (_, _, with) = send_latest(&views, "/Items/Latest", false).await;
    assert!(with[0].image_tags.is_some(), "images are on by default");
    let (_, _, without) = send_latest(&views, "/Items/Latest?enableImages=false", false).await;
    assert!(without[0].image_tags.is_none());
}

/// The user's `LatestItemExcludes` (read off the user DTO's configuration)
/// reach the manager — drop the `get_user_dto` lookup and this fails.
#[tokio::test]
async fn latest_forwards_the_users_latest_item_excludes() {
    let views = LatestViews::new(Vec::new());
    let excluded = Uuid::from_u128(0xE8C1);
    let router = create_router(latest_state(Arc::clone(&views), false, vec![excluded]));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/Items/Latest")
                .header("Authorization", "Token abc")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let q = views
        .recorded
        .lock()
        .expect("lock")
        .clone()
        .expect("the manager was called");
    assert_eq!(q.latest_item_excludes, vec![excluded]);
}

// ---- GET /UserItems/Resume: the two v12 query inputs -----------------------

/// The alternate version Alice's session is playing.
const ALTERNATE_ID: Uuid = Uuid::from_u128(0xA17);
/// Its primary — the group root `GetAllVersions()` spans from.
const PRIMARY_ID: Uuid = Uuid::from_u128(0xA10);
/// The primary's other alternate, which must be excluded too.
const SIBLING_ID: Uuid = Uuid::from_u128(0xA18);
/// What some other user's session plays — never excluded for Alice.
const OTHER_USERS_ITEM_ID: Uuid = Uuid::from_u128(0x0BAD);
/// Alice's visible root folders: two libraries and the playlists folder.
const RESUME_MOVIES_ID: Uuid = Uuid::from_u128(0xF001);
const RESUME_SHOWS_ID: Uuid = Uuid::from_u128(0xF002);
const RESUME_PLAYLISTS_ID: Uuid = Uuid::from_u128(0xF003);

/// A [`LibraryManager`] recording the resume `InternalItemsQuery`, answering
/// the `user_root_children` query with Alice's root folders (plus a non-folder
/// row the `i is Folder` filter must drop), and resolving the alternate to a
/// row that names its primary.
#[derive(Default)]
struct RecordingResumeLibrary {
    recorded: Mutex<Option<InternalItemsQuery>>,
}

#[async_trait]
impl LibraryManager for RecordingResumeLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id == ALTERNATE_ID {
            let mut row = item_entity(ALTERNATE_ID, "Movie - 1080p", BaseItemKind::Video);
            row.primary_version_id = Some(PRIMARY_ID.to_string());
            return Ok(Some(row));
        }
        Ok((id == ITEM_ID).then(|| item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)))
    }
    async fn query_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        // The user-root children answer only for a USER-scoped query: the
        // repository applies `Folder.IsVisible` only when `filter.user` is
        // set, so a handler that dropped the user would get an empty list.
        if query.user_root_children
            && query
                .user
                .as_ref()
                .is_some_and(|u| u.id == USER_ID.to_string())
        {
            let mut not_a_folder =
                item_entity(Uuid::from_u128(0xF0FF), "Loose", BaseItemKind::Movie);
            not_a_folder.is_folder = false;
            let folder = |id, name, kind| {
                let mut row = item_entity(id, name, kind);
                row.is_folder = true;
                row
            };
            return Ok(QueryResult::from_items(vec![
                folder(RESUME_MOVIES_ID, "Movies", BaseItemKind::CollectionFolder),
                folder(RESUME_SHOWS_ID, "Shows", BaseItemKind::CollectionFolder),
                folder(
                    RESUME_PLAYLISTS_ID,
                    "Playlists",
                    BaseItemKind::PlaylistsFolder,
                ),
                not_a_folder,
            ]));
        }
        *self.recorded.lock().expect("lock") = Some(query.clone());
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
        ))
    }
    async fn get_item_list(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
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
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
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
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
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

/// A [`SessionManager`] whose only live method is `get_sessions`.
struct ResumeSessions;

#[async_trait]
impl SessionManager for ResumeSessions {
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
        // Alice's session plays the ALTERNATE version; another user's session
        // plays something else and must be ignored.
        let playing = |user_id: Uuid, item: Uuid| SessionInfoDto {
            user_id,
            now_playing_item: Some(BaseItemDto {
                id: item,
                ..BaseItemDto::default()
            }),
            ..SessionInfoDto::default()
        };
        Ok(vec![
            playing(USER_ID, ALTERNATE_ID),
            playing(Uuid::from_u128(0x0B0B), OTHER_USERS_ITEM_ID),
        ])
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

/// A [`MediaSourceManager`] whose only live method is the alternate-version
/// batch: the primary has two alternates.
struct ResumeMediaSources;

#[async_trait]
impl MediaSourceManager for ResumeMediaSources {
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
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_alternate_versions_batch(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        // Only the primary has alternates: the playing one and its sibling.
        Ok(primary_ids
            .iter()
            .filter(|id| **id == PRIMARY_ID)
            .map(|id| {
                (
                    *id,
                    vec![
                        item_entity(ALTERNATE_ID, "Movie - 1080p", BaseItemKind::Video),
                        item_entity(SIBLING_ID, "Movie - 720p", BaseItemKind::Video),
                    ],
                )
            })
            .collect())
    }
}

/// Builds the Resume state: recording library, Alice's excludes, one playing
/// session, and a two-alternate version group.
fn resume_state(excludes: Vec<Uuid>) -> (AppState, Arc<RecordingResumeLibrary>) {
    let library = Arc::new(RecordingResumeLibrary::default());
    let state = AppState::new(
        Arc::clone(&library) as Arc<dyn LibraryManager>,
        Arc::new(OkUsers {
            latest_item_excludes: excludes,
        }),
        Arc::new(StubUserViews),
        Arc::new(RecordingUserData::default()),
        Arc::new(ResumeMediaSources),
        Arc::new(ResumeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(LatestDto),
        Arc::new(OkAuth { elevated: false }),
        Arc::new(OkAuth { elevated: false }),
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
    .with_virtual_folders(Arc::new(virtual_folders()));
    (state, library)
}

async fn send_resume(
    excludes: Vec<Uuid>,
    uri: &str,
) -> (StatusCode, Vec<u8>, Option<InternalItemsQuery>) {
    let (state, library) = resume_state(excludes);
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("Authorization", "Token abc")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    let recorded = library.recorded.lock().expect("lock").clone();
    (status, body, recorded)
}

/// v12 ItemsController.cs:952-961: an unscoped request by a user with
/// `LatestItemExcludes` is confined to the user-root folders (`i is Folder`)
/// the user can see, minus the excluded ones — and only then.
#[tokio::test]
async fn resume_confines_an_unscoped_request_to_the_users_remaining_root_folders() {
    let (status, _, recorded) = send_resume(vec![RESUME_SHOWS_ID], "/UserItems/Resume").await;
    assert_eq!(status, StatusCode::OK);
    let query = recorded.expect("the resume query ran");
    assert_eq!(
        query.ancestor_ids,
        vec![RESUME_MOVIES_ID, RESUME_PLAYLISTS_ID]
    );
    assert_eq!(query.is_resumable, Some(true));

    // A scoped request (`parentId`) leaves AncestorIds alone…
    let (_, _, recorded) = send_resume(
        vec![RESUME_SHOWS_ID],
        &format!("/UserItems/Resume?parentId={RESUME_MOVIES_ID}"),
    )
    .await;
    let query = recorded.expect("ran");
    assert!(query.ancestor_ids.is_empty());
    assert_eq!(query.parent_id, RESUME_MOVIES_ID);
    // …and so does a user with no excludes.
    let (_, _, recorded) = send_resume(Vec::new(), "/UserItems/Resume").await;
    assert!(recorded.expect("ran").ancestor_ids.is_empty());
}

/// v12 ItemsController.cs:963-976: `excludeActiveSessions` drops every
/// version of what the user's own sessions are playing — the alternate that
/// is playing, its primary, and the primary's other alternate — and nothing
/// another user is playing.
#[tokio::test]
async fn resume_excludes_every_version_of_what_the_users_sessions_play() {
    let (status, _, recorded) =
        send_resume(Vec::new(), "/UserItems/Resume?excludeActiveSessions=true").await;
    assert_eq!(status, StatusCode::OK);
    let mut excluded = recorded.expect("ran").exclude_item_ids;
    excluded.sort_unstable();
    let mut expected = vec![ALTERNATE_ID, PRIMARY_ID, SIBLING_ID];
    expected.sort_unstable();
    assert_eq!(excluded, expected);

    // Off by default.
    let (_, _, recorded) = send_resume(Vec::new(), "/UserItems/Resume").await;
    assert!(recorded.expect("ran").exclude_item_ids.is_empty());
}

/// `AddAdditionalDtoOptions` reaches the projection: `enableImages=false`
/// strips the image tags the DTO fake otherwise emits.
#[tokio::test]
async fn resume_forwards_the_image_toggles_to_the_projection() {
    let (status, body, _) = send_resume(Vec::new(), "/UserItems/Resume?enableImages=false").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert!(json["Items"][0].get("ImageTags").is_none(), "{json}");
    let (_, body, _) = send_resume(Vec::new(), "/UserItems/Resume").await;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert!(json["Items"][0].get("ImageTags").is_some(), "{json}");
}
