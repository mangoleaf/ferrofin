//! Batch-14 handler **success-path** tests: library theme-media / file serve /
//! scan trigger + item external-id descriptors.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! compact `hermit-traits` stubs that authenticate as a fixed user and return
//! canned data; every manager a given handler does not touch reuses the
//! `test_support` panic fakes, so a handler that strays into an unexpected seam
//! trips a panic. The deferred routes (virtual-folders, remote-search, the
//! external-source change reports) are covered by the contract-superset test as
//! still-registered `501` stubs, not here.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeConfig,
    FakeDevices, FakeDisplayPreferences, FakeFileSystem, FakeLocalization, FakeLyrics,
    FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeQuickConnect, FakeSearch,
    FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem, FakeTasks, FakeTrickplay,
    FakeTvSeries, FakeUserData, FakeUserViews, minimal_base_item,
};
use hermit_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::{MetadataOptions, MetadataPluginSummary, UserConfiguration};
use hermit_model::data::CollectionType;
use hermit_model::dto::{BaseItemDto, UserDto};
use hermit_model::entities::{ExtraType, ImageType};
use hermit_model::providers::{
    ExternalIdInfo, ExternalUrl, ImageProviderInfo, RemoteImageInfo, RemoteImageQuery,
};
use hermit_model::querying::{AllThemeMediaResult, QueryResult, ThemeMediaResult};
use hermit_model::users::UserPolicy;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, UserManager};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::providers::{
    ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
const ITEM_ID: Uuid = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
const SONG_ID: Uuid = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
const VIDEO_ID: Uuid = Uuid::from_u128(0x4444_4444_4444_4444_4444_4444_4444_4444);
const ROOT_ID: Uuid = Uuid::from_u128(0x5555_5555_5555_5555_5555_5555_5555_5555);

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

/// A [`LibraryManager`] resolving [`ITEM_ID`] (with an on-disk path), the root
/// folder, and returning a theme-song / theme-video extra keyed by `extra_types`.
struct StubLibrary;

#[async_trait]
impl LibraryManager for StubLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id == ITEM_ID {
            let mut item = minimal_base_item(ITEM_ID, "Movie", "Movie");
            item.path = Some("/does/not/matter.mkv".to_owned());
            Ok(Some(item))
        } else {
            Ok(None)
        }
    }
    async fn get_user_root_folder(&self) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(Some(minimal_base_item(
            ROOT_ID,
            "Media Folders",
            "UserRootFolder",
        )))
    }
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if query.extra_types.contains(&ExtraType::ThemeSong) {
            Ok(vec![minimal_base_item(SONG_ID, "Theme Song", "Audio")])
        } else if query.extra_types.contains(&ExtraType::ThemeVideo) {
            Ok(vec![minimal_base_item(VIDEO_ID, "Theme Video", "Video")])
        } else {
            Ok(Vec::new())
        }
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn query_items(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _q: &InternalItemsQuery,
        _c: CollectionType,
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
    ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<hermit_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _t: hermit_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
}

/// A [`UserManager`] resolving the fixed authenticated user.
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
    async fn get_user_by_name(&self, _n: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!()
    }
    async fn rename_user(&self, _i: Uuid, _o: &str, _n: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_user(&self, _u: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn create_user(&self, _n: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!()
    }
    async fn delete_user(&self, _i: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn reset_password(&self, _i: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn change_password(&self, _i: Uuid, _p: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn authenticate_user(
        &self,
        _u: &str,
        _p: &str,
        _r: &str,
        _s: bool,
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
        _u: &UserEntity,
        _s: Option<String>,
    ) -> Result<UserDto, ServiceError> {
        unimplemented!()
    }
    async fn update_configuration(
        &self,
        _i: Uuid,
        _c: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(&self, _i: Uuid, _p: &UserPolicy) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_profile_image(&self, _u: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`DtoService`] projecting each entity to a minimal [`BaseItemDto`] carrying
/// its id, so the theme-media assertions can match on it.
struct OkDto;

fn entity_to_dto(item: &BaseItemEntity) -> BaseItemDto {
    BaseItemDto {
        id: Uuid::parse_str(&item.id).unwrap_or_default(),
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
        _o: &DtoOptions,
        _u: Option<&UserEntity>,
        _owner: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _o: &DtoOptions,
        _u: Option<&UserEntity>,
        _owner: Option<Uuid>,
        _skip: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items.iter().map(entity_to_dto).collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _o: &DtoOptions,
        _tagged: Option<&[Uuid]>,
        _u: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
}

/// A [`ProviderManager`] advertising a single external-id descriptor.
struct OkProviders;

#[async_trait]
impl ProviderManager for OkProviders {
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError> {
        Ok(vec![ExternalIdInfo::new(
            "Tmdb".to_owned(),
            "Tmdb".to_owned(),
            None,
        )])
    }
    async fn queue_refresh(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
        _p: RefreshPriority,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_full_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_single_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        unimplemented!()
    }
    async fn save_image_from_url(
        &self,
        _i: Uuid,
        _u: &str,
        _t: ImageType,
        _x: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn save_image(
        &self,
        _i: Uuid,
        _c: &[u8],
        _m: &str,
        _t: ImageType,
        _x: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_available_remote_images(
        &self,
        _i: Uuid,
        _q: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_remote_image_provider_info(
        &self,
        _i: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        unimplemented!()
    }
    async fn save_metadata(&self, _i: Uuid, _u: ItemUpdateType) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_external_urls(&self, _i: Uuid) -> Result<Vec<ExternalUrl>, ServiceError> {
        unimplemented!()
    }
    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError> {
        unimplemented!()
    }
    async fn get_metadata_options(&self, _i: Uuid) -> Result<MetadataOptions, ServiceError> {
        unimplemented!()
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
}

/// An auth pair that authenticates every request as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// Builds an [`AppState`] wired with the batch-14 stubs.
fn state() -> AppState {
    AppState::new(
        Arc::new(StubLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(OkProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeDevices),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeApiKeys),
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Drives one request through the router and returns (status, body bytes).
async fn send(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    use tower::ServiceExt;
    let router = create_router(state());
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
async fn theme_songs_returns_song_extra() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ThemeSongs"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: ThemeMediaResult = serde_json::from_slice(&body).expect("theme media result");
    assert_eq!(result.owner_id, ITEM_ID);
    assert_eq!(result.result.items.len(), 1);
    assert_eq!(result.result.items[0].id, SONG_ID);
    assert_eq!(result.result.total_record_count, 1);
}

#[tokio::test]
async fn theme_videos_returns_video_extra() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ThemeVideos"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: ThemeMediaResult = serde_json::from_slice(&body).expect("theme media result");
    assert_eq!(result.result.items.len(), 1);
    assert_eq!(result.result.items[0].id, VIDEO_ID);
}

#[tokio::test]
async fn theme_media_combines_songs_and_videos() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ThemeMedia"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: AllThemeMediaResult = serde_json::from_slice(&body).expect("all theme media");
    assert_eq!(result.theme_songs_result.result.items[0].id, SONG_ID);
    assert_eq!(result.theme_videos_result.result.items[0].id, VIDEO_ID);
    assert!(result.soundtrack_songs_result.result.items.is_empty());
}

#[tokio::test]
async fn theme_songs_missing_item_is_404() {
    let missing = Uuid::from_u128(0x9999_9999_9999_9999_9999_9999_9999_9999);
    let (status, _) = send(
        "GET",
        &format!("/Items/{missing}/ThemeSongs"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn file_missing_item_is_404() {
    let missing = Uuid::from_u128(0x9999_9999_9999_9999_9999_9999_9999_9999);
    let (status, _) = send("GET", &format!("/Items/{missing}/File"), Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn refresh_library_returns_204() {
    let (status, _) = send("POST", "/Library/Refresh", Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn external_id_infos_returns_descriptor() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ExternalIdInfos"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let infos: Vec<ExternalIdInfo> = serde_json::from_slice(&body).expect("external id infos");
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name.as_deref(), Some("Tmdb"));
}

#[tokio::test]
async fn external_id_infos_missing_item_is_404() {
    let missing = Uuid::from_u128(0x9999_9999_9999_9999_9999_9999_9999_9999);
    let (status, _) = send(
        "GET",
        &format!("/Items/{missing}/ExternalIdInfos"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
