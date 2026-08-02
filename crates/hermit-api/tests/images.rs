//! Image integration tests: item images, by-name images, the item image-info
//! list, user profile images, remote (provider) images, item image write/delete/
//! reorder, user-image upload, and TMDb image configuration.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate as a fixed user and return (or
//! record) canned data. Image serving is exercised against a real temp file so
//! the `200` + body assertion covers the `ServeFile` tail; resolution/`404`
//! outcomes are asserted with canned image rows. Managers a given handler never
//! touches reuse the `test_support` panic fakes, catching a handler that strays.

use std::sync::{Arc, Mutex};

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
    FakeTvSeries, FakeUserData, FakeUserViews,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::entities::ImageType;
use hermit_model::providers::{ImageProviderInfo, RemoteImageInfo, RemoteImageQuery};
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, UserManager};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{AuthorizationInfo, ItemImageInfo};
use hermit_traits::providers::{
    ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
const ITEM_ID: Uuid = Uuid::from_u128(0x00A1_1A6E);
const GENRE_ID: Uuid = Uuid::from_u128(0x0D_6E_46);
const MISSING_ID: Uuid = Uuid::from_u128(0xDEAD);

/// A unique on-disk PNG-ish file for the serve-path tests, removed on drop.
struct TempImage {
    path: std::path::PathBuf,
}

impl TempImage {
    fn new(bytes: &[u8]) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("hermit-img-{}.bin", Uuid::new_v4()));
        std::fs::write(&path, bytes).expect("write temp image");
        Self { path }
    }

    fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Builds a minimal [`UserEntity`]; every non-id field is a neutral zero value.
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

/// Builds a minimal [`BaseItemEntity`] carrying id + name + type.
fn item_entity(id: Uuid, name: &str, kind: &str) -> BaseItemEntity {
    BaseItemEntity {
        id: id.to_string(),
        clean_name: Some(name.to_lowercase()),
        name: Some(name.to_owned()),
        presentation_unique_key: Some(format!("key-{name}")),
        type_: kind.to_owned(),
        ..empty_item()
    }
}

/// A `BaseItemEntity` with every optional field `None`/zero, so `item_entity`
/// only sets the handful of fields the tests read.
fn empty_item() -> BaseItemEntity {
    BaseItemEntity {
        id: String::new(),
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
        name: None,
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

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
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

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
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

/// A [`LibraryManager`] returning a fixed item, its images, and a by-name genre,
/// and recording each `swap_images` reorder the handler asks for.
///
/// `image_path` is the on-disk file the item's Primary image points at; the
/// by-name genre carries the same image so the by-name serve path is covered.
struct StubLibrary {
    image_path: String,
    /// Records each `(item_id, image_type, index1, index2)` swap the handler asks
    /// for, so the reorder test can assert the request reached the manager.
    swaps: Mutex<Vec<(Uuid, ImageType, i32, i32)>>,
}

#[async_trait]
impl LibraryManager for StubLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(match id {
            _ if id == ITEM_ID => Some(item_entity(ITEM_ID, "Imaged Movie", "Movie")),
            _ if id == GENRE_ID => Some(item_entity(GENRE_ID, "Drama", "Genre")),
            _ => None,
        })
    }

    async fn get_item_images(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
        if item_id == ITEM_ID || item_id == GENRE_ID {
            Ok(vec![
                ItemImageInfo {
                    path: self.image_path.clone(),
                    image_type: ImageType::Primary,
                    width: 100,
                    height: 200,
                    blur_hash: Some("LKO2".to_owned()),
                    ..ItemImageInfo::default()
                },
                ItemImageInfo {
                    path: "https://remote.example/backdrop.jpg".to_owned(),
                    image_type: ImageType::Backdrop,
                    ..ItemImageInfo::default()
                },
            ])
        } else {
            Ok(vec![])
        }
    }

    async fn swap_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError> {
        // Mirror the real manager's 400 guard so the "wrong type" test exercises
        // it, then record the accepted swap.
        if !matches!(image_type, ImageType::Backdrop | ImageType::Chapter) {
            return Err(ServiceError::invalid_input("not reorderable"));
        }
        self.swaps
            .lock()
            .expect("lock")
            .push((item_id, image_type, index1, index2));
        Ok(())
    }

    async fn get_named_item(
        &self,
        _kind: BaseItemKind,
        name: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((name == "Drama").then(|| item_entity(GENRE_ID, "Drama", "Genre")))
    }

    async fn query_items(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<hermit_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_list(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
        _c: hermit_model::data::CollectionType,
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
    async fn delete_item(
        &self,
        _id: Uuid,
        _o: &hermit_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &hermit_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_people_names(
        &self,
        _q: &hermit_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<
        hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: hermit_model::entities::MediaStreamType,
        _q: &hermit_traits::options::InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`UserManager`] resolving the fixed user, its profile image, and recording
/// `save_profile_image`.
struct StubUsers {
    profile_path: String,
    saved: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl UserManager for StubUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(|| user_entity(USER_ID, "alice")))
    }

    async fn get_profile_image(
        &self,
        user_id: Uuid,
    ) -> Result<Option<ItemImageInfo>, ServiceError> {
        Ok((user_id == USER_ID).then(|| ItemImageInfo {
            path: self.profile_path.clone(),
            image_type: ImageType::Profile,
            ..ItemImageInfo::default()
        }))
    }

    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn save_profile_image(
        &self,
        user: &UserEntity,
        _content: &[u8],
        mime_type: &str,
        _extension: &str,
    ) -> Result<(), ServiceError> {
        self.saved
            .lock()
            .expect("lock")
            .push((user.id.clone(), mime_type.to_owned()));
        Ok(())
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
    async fn delete_user(&self, _id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn reset_password(&self, _id: Uuid) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn change_password(&self, _id: Uuid, _new: &str) -> Result<(), ServiceError> {
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
        _user: &UserEntity,
        _remote_endpoint: Option<String>,
    ) -> Result<hermit_model::dto::UserDto, ServiceError> {
        unimplemented!()
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
}

/// A [`ProviderManager`] recording `save_image`/`delete_image` and returning a
/// canned remote image + provider.
struct StubProviders {
    saved: Arc<Mutex<Vec<(Uuid, String)>>>,
    deleted: Arc<Mutex<Vec<(Uuid, i32)>>>,
}

#[async_trait]
impl ProviderManager for StubProviders {
    async fn queue_refresh(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        unimplemented!()
    }
    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn save_image(
        &self,
        item_id: Uuid,
        _content: &[u8],
        mime_type: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        self.saved
            .lock()
            .expect("lock")
            .push((item_id, mime_type.to_owned()));
        Ok(())
    }
    async fn delete_image(
        &self,
        item_id: Uuid,
        _image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        self.deleted
            .lock()
            .expect("lock")
            .push((item_id, image_index.unwrap_or(0)));
        Ok(())
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        Ok(vec![RemoteImageInfo {
            provider_name: Some("TheMovieDb".to_owned()),
            url: Some("https://img.example/1.jpg".to_owned()),
            type_: ImageType::Primary,
            ..RemoteImageInfo::default()
        }])
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        Ok(vec![ImageProviderInfo::new(
            "TheMovieDb".to_owned(),
            vec![ImageType::Primary, ImageType::Backdrop],
        )])
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        Ok(())
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

/// Bundles the image stubs and their recording handles for one test.
struct Stubs {
    library: Arc<StubLibrary>,
    users: Arc<StubUsers>,
    providers: Arc<StubProviders>,
}

/// Builds the image stubs, serving `image_path` for item/by-name images and
/// `profile_path` for the user image; write routes record into the stub mutexes.
fn stubs(image_path: String, profile_path: String) -> Stubs {
    Stubs {
        library: Arc::new(StubLibrary {
            image_path,
            swaps: Mutex::new(Vec::new()),
        }),
        users: Arc::new(StubUsers {
            profile_path,
            saved: Arc::new(Mutex::new(Vec::new())),
        }),
        providers: Arc::new(StubProviders {
            saved: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
    }
}

/// Builds an [`AppState`] wired with the image stubs.
fn state(s: &Stubs) -> AppState {
    AppState::new(
        s.library.clone(),
        s.users.clone(),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        s.providers.clone(),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(hermit_api::test_support::FakeDto),
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

/// Drives one authenticated request through the router, with an optional
/// `(content_type, payload)` body.
async fn send(
    s: &Stubs,
    method: &str,
    uri: &str,
    body: Option<(&str, &str)>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Bearer token");
    let request = if let Some((content_type, payload)) = body {
        builder = builder.header("Content-Type", content_type);
        builder
            .body(Body::from(payload.to_owned()))
            .expect("request")
    } else {
        builder.body(Body::empty()).expect("request")
    };
    let response = create_router(state(s))
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, bytes)
}

// ---- item image serve (batch9) ------------------------------------------------

#[tokio::test]
async fn get_item_image_serves_the_local_file() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images/Primary"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"PNGDATA");
}

#[tokio::test]
async fn head_item_image_is_ok_without_body() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let (status, body) = send(
        &s,
        "HEAD",
        &format!("/Items/{ITEM_ID}/Images/Primary"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn item_image_by_index_serves_the_file() {
    let img = TempImage::new(b"IDX0");
    let s = stubs(img.path(), String::new());
    let (status, body) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Primary/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"IDX0");
}

#[tokio::test]
async fn item_image_missing_item_is_404() {
    let s = stubs(String::new(), String::new());
    let missing = Uuid::from_u128(0xDEAD);
    let (status, _) = send(&s, "GET", &format!("/Items/{missing}/Images/Primary"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn item_image_wrong_type_is_404() {
    let img = TempImage::new(b"X");
    let s = stubs(img.path(), String::new());
    // The item has no Logo image → 404.
    let (status, _) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images/Logo"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn remote_image_backdrop_is_404_no_local_file() {
    let img = TempImage::new(b"X");
    let s = stubs(img.path(), String::new());
    // The Backdrop image is a remote URL; with no image processor it cannot be
    // served locally → 404.
    let (status, _) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Backdrop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_image_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images/Bogus"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn item_image_infos_lists_images() {
    let img = TempImage::new(b"X");
    let s = stubs(img.path(), String::new());
    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images"), None).await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let arr = value.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    // Primary (single-instance) carries no index; Backdrop (multi) carries 0.
    assert_eq!(arr[0]["ImageType"], "Primary");
    assert!(arr[0].get("ImageIndex").is_none());
    assert_eq!(arr[0]["Width"], 100);
    assert_eq!(arr[0]["BlurHash"], "LKO2");
    assert_eq!(arr[1]["ImageType"], "Backdrop");
    assert_eq!(arr[1]["ImageIndex"], 0);
}

#[tokio::test]
async fn by_name_genre_image_serves_the_file() {
    let img = TempImage::new(b"GENREIMG");
    let s = stubs(img.path(), String::new());
    let (status, body) = send(&s, "GET", "/Genres/Drama/Images/Primary", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"GENREIMG");
}

#[tokio::test]
async fn by_name_missing_is_404() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "GET", "/Genres/Nonexist/Images/Primary", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn user_image_serves_profile() {
    let img = TempImage::new(b"PROFILE");
    let s = stubs(String::new(), img.path());
    let (status, body) = send(&s, "GET", "/UserImage", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"PROFILE");
}

#[tokio::test]
async fn delete_user_image_is_204() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "DELETE", "/UserImage", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn remote_images_returns_result() {
    let s = stubs(String::new(), String::new());
    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/RemoteImages"), None).await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(value["TotalRecordCount"], 1);
    assert_eq!(value["Images"][0]["ProviderName"], "TheMovieDb");
    assert_eq!(value["Providers"][0], "TheMovieDb");
}

#[tokio::test]
async fn remote_image_providers_returns_list() {
    let s = stubs(String::new(), String::new());
    let (status, body) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/RemoteImages/Providers"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(value[0]["Name"], "TheMovieDb");
}

#[tokio::test]
async fn download_remote_image_is_204() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/RemoteImages/Download?type=Primary&imageUrl=https://x/y.jpg"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn download_remote_image_missing_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/RemoteImages/Download?imageUrl=https://x/y.jpg"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---- item image write/delete (batch16) ----------------------------------------

#[tokio::test]
async fn set_item_image_saves_and_returns_204() {
    let s = stubs(String::new(), String::new());
    // "hi" base64-encoded.
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/Images/Primary"),
        Some(("image/png", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let saved = s.providers.saved.lock().expect("lock");
    assert_eq!(saved.as_slice(), [(ITEM_ID, "image/png".to_owned())]);
}

#[tokio::test]
async fn set_item_image_by_index_saves() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/Images/Backdrop/2"),
        Some(("image/jpeg", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(s.providers.saved.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn set_item_image_bad_content_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/Images/Primary"),
        Some(("application/json", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(s.providers.saved.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn set_item_image_missing_item_is_404() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{MISSING_ID}/Images/Primary"),
        Some(("image/png", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_item_image_returns_204() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "DELETE",
        &format!("/Items/{ITEM_ID}/Images/Primary?imageIndex=3"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.providers.deleted.lock().expect("lock").as_slice(),
        [(ITEM_ID, 3)]
    );
}

#[tokio::test]
async fn delete_item_image_by_index_returns_204() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "DELETE",
        &format!("/Items/{ITEM_ID}/Images/Backdrop/1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.providers.deleted.lock().expect("lock").as_slice(),
        [(ITEM_ID, 1)]
    );
}

#[tokio::test]
async fn delete_item_image_missing_item_is_404() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "DELETE",
        &format!("/Items/{MISSING_ID}/Images/Primary"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- item image reorder (UpdateItemImageIndex) --------------------------------

#[tokio::test]
async fn update_item_image_index_swaps_and_returns_204() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/Images/Backdrop/1/Index?newIndex=3"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.library.swaps.lock().expect("lock").as_slice(),
        [(ITEM_ID, ImageType::Backdrop, 1, 3)]
    );
}

#[tokio::test]
async fn update_item_image_index_non_multiple_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/Images/Primary/0/Index?newIndex=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(s.library.swaps.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn update_item_image_index_missing_item_is_404() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{MISSING_ID}/Images/Backdrop/0/Index?newIndex=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(s.library.swaps.lock().expect("lock").is_empty());
}

// ---- TMDb client configuration ------------------------------------------------

#[tokio::test]
async fn tmdb_client_configuration_returns_image_config() {
    let s = stubs(String::new(), String::new());
    let (status, body) = send(&s, "GET", "/Tmdb/ClientConfiguration", None).await;
    assert_eq!(status, StatusCode::OK);
    let config: hermit_model::dto::ConfigImageTypes =
        serde_json::from_slice(&body).expect("config");
    assert_eq!(
        config.secure_base_url.as_deref(),
        Some("https://image.tmdb.org/t/p/")
    );
    assert!(
        config
            .poster_sizes
            .as_deref()
            .expect("poster sizes")
            .contains(&"original".to_owned())
    );
}

// ---- user image upload --------------------------------------------------------

#[tokio::test]
async fn post_user_image_saves_and_returns_204() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "POST", "/UserImage", Some(("image/png", "aGk="))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let saved = s.users.saved.lock().expect("lock");
    assert_eq!(
        saved.as_slice(),
        [(USER_ID.to_string(), "image/png".to_owned())]
    );
}

#[tokio::test]
async fn post_user_image_bad_content_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "POST", "/UserImage", Some(("text/plain", "aGk="))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---- image missing / bad type (handler_success_paths) -------------------------

#[tokio::test]
async fn item_image_missing_is_404() {
    // No image processor is wired, so a valid item + valid image type still 404s
    // (there is no image path to serve) — the contract's not-found outcome.
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images/Primary"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn item_image_bad_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/NotAnImageType"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
