//! Image integration tests: item images, by-name images, the item image-info
//! list, user profile images, remote (provider) images, item image write/delete/
//! reorder, user-image upload, and TMDb image configuration.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate as a fixed user and return (or
//! record) canned data. Image serving is exercised against a real temp file so
//! the `200` + body assertion covers the `ServeFile` tail; resolution/`404`
//! outcomes are asserted with canned image rows. Managers a given handler never
//! touches reuse the `test_support` panic fakes, catching a handler that strays.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeConfig,
    FakeDevices, FakeDisplayPreferences, FakeFileSystem, FakeLocalization, FakeLyrics,
    FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeQuickConnect, FakeSearch,
    FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem, FakeTasks, FakeTrickplay,
    FakeTvSeries, FakeUserData, FakeUserViews, fake_user_entity,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_model::providers::{ImageProviderInfo, RemoteImageInfo, RemoteImageQuery};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{AuthorizationInfo, ItemImageInfo, ItemImagesToServe};
use ferrofin_traits::providers::{
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
        path.push(format!("ferrofin-img-{}.bin", Uuid::new_v4()));
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
        password: Some("hashed".to_owned()),
        ..fake_user_entity(id, username)
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

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
///
/// `elevated` authenticates as an API key instead, which satisfies the
/// `RequiresElevation` policy on the image *write* routes (upload, delete,
/// reorder) without a user/policy lookup. It is deliberately NOT the default:
/// the read routes must keep proving they work for an ordinary user, since
/// over-gating them would break every client's thumbnail grid.
struct OkAuth {
    elevated: bool,
    /// The caller's `EnableUserPreferenceAccess` is off (`AssertCanUpdateUser`).
    restricted: bool,
    /// The caller is an API key with no user, so its effective user id is nil.
    userless: bool,
}

impl OkAuth {
    fn caller(&self) -> Option<UserEntity> {
        (!self.userless).then(|| UserEntity {
            enable_user_preference_access: !self.restricted,
            ..user_entity(USER_ID, "alice")
        })
    }
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        if !request
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(ServiceError::unauthorized("no credentials"));
        }
        Ok(AuthorizationInfo {
            user: self.caller().map(Arc::new),
            is_api_key: self.elevated || self.userless,
            is_authenticated: true,
            ..ferrofin_api::test_support::authenticated_user_info()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        if !request
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(ServiceError::unauthorized("no credentials"));
        }
        Ok(AuthorizationInfo {
            user: self.caller().map(Arc::new),
            is_api_key: self.elevated || self.userless,
            is_authenticated: true,
            ..ferrofin_api::test_support::authenticated_user_info()
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
    /// Counts every item-existence read the handler performs, by either route
    /// (`get_item_by_id` or `item_exists`). The served path must make none: the
    /// image row it found already proves the item exists.
    item_reads: Mutex<usize>,
    /// Whether [`ITEM_ID`] is a photo whose images are auto-oriented.
    photo: bool,
    /// Whether [`ITEM_ID`] is a Live TV channel.
    live_tv: bool,
}

impl StubLibrary {
    /// How many item-existence reads the handlers have made so far.
    fn item_reads(&self) -> usize {
        *self.item_reads.lock().expect("lock")
    }
}

#[async_trait]
impl LibraryManager for StubLibrary {
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
        *self.item_reads.lock().expect("lock") += 1;
        Ok(match id {
            _ if id == ITEM_ID && self.live_tv => Some(item_entity(
                ITEM_ID,
                "Channel",
                BaseItemKind::LiveTvChannel
                    .stored_type_name()
                    .expect("stored name"),
            )),
            _ if id == ITEM_ID => Some(item_entity(ITEM_ID, "Imaged Movie", "Movie")),
            _ if id == GENRE_ID => Some(item_entity(GENRE_ID, "Drama", "Genre")),
            _ => None,
        })
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        // Counted through the same door as `get_item_by_id`: the assertion is
        // about *any* item read on the hot path, not about which one.
        *self.item_reads.lock().expect("lock") += 1;
        Ok(id == ITEM_ID || id == GENRE_ID)
    }

    async fn get_item_images_to_serve(
        &self,
        item_id: Uuid,
    ) -> Result<ItemImagesToServe, ServiceError> {
        Ok(ItemImagesToServe {
            images: self.get_item_images(item_id).await?,
            auto_orient: self.photo && item_id == ITEM_ID,
            live_tv: self.live_tv && item_id == ITEM_ID,
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

    async fn get_chapter_image(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ItemImageInfo>, ServiceError> {
        // One chapter, at index 0, whose thumbnail is the fixture image.
        Ok((item_id == ITEM_ID && index == 0).then(|| ItemImageInfo {
            path: self.image_path.clone(),
            image_type: ImageType::Chapter,
            ..ItemImageInfo::default()
        }))
    }

    async fn swap_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<bool, ServiceError> {
        // Mirror the real manager's 400 guard so the "wrong type" test exercises
        // it, then record the accepted swap.
        if !matches!(image_type, ImageType::Backdrop | ImageType::Chapter) {
            return Err(ServiceError::invalid_input("not reorderable"));
        }
        self.swaps
            .lock()
            .expect("lock")
            .push((item_id, image_type, index1, index2));
        // Index 99 stands for a slot that does not exist: upstream does nothing.
        Ok(index2 != 99)
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
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_list(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
        _c: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
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
        _o: &ferrofin_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_people_names(
        &self,
        _q: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _q: &ferrofin_traits::options::InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// One recorded `save_profile_image` call: user id, MIME type, extension.
type SavedProfileImage = (String, String, String);

/// A [`UserManager`] resolving the fixed user, its profile image, and recording
/// `save_profile_image` and `clear_profile_image`.
struct StubUsers {
    profile_path: String,
    saved: Arc<Mutex<Vec<SavedProfileImage>>>,
    cleared: Mutex<usize>,
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
        *self.cleared.lock().expect("lock") += 1;
        Ok(())
    }

    async fn save_profile_image(
        &self,
        user: &UserEntity,
        _content: &[u8],
        mime_type: &str,
        extension: &str,
    ) -> Result<(), ServiceError> {
        self.saved.lock().expect("lock").push((
            user.id.clone(),
            mime_type.to_owned(),
            extension.to_owned(),
        ));
        Ok(())
    }

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
        _remote_endpoint: Option<String>,
    ) -> Result<ferrofin_model::dto::UserDto, ServiceError> {
        // The default policy: the caller is not an administrator.
        Ok(ferrofin_model::dto::UserDto::default())
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
}

type SavedImage = (Uuid, String, ImageType, Option<i32>);

/// A [`ProviderManager`] recording `save_image`/`delete_image` and returning a
/// canned remote image + provider.
struct StubProviders {
    saved: Arc<Mutex<Vec<SavedImage>>>,
    deleted: Arc<Mutex<Vec<(Uuid, i32)>>>,
    /// The `ImageUpdate` repository saves requested through `update_to_repository`.
    updates: Arc<Mutex<Vec<Uuid>>>,
    /// The local copy `cached_remote_image` hands back, and the paths it was
    /// asked for. `None` → no copy can be made.
    remote_copy: Option<String>,
    remote_asked: Arc<Mutex<Vec<String>>>,
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
    async fn cached_remote_image(&self, path: &str) -> Result<String, ServiceError> {
        self.remote_asked
            .lock()
            .expect("lock")
            .push(path.to_owned());
        self.remote_copy
            .clone()
            .ok_or_else(|| ServiceError::backend("unreachable"))
    }
    async fn save_image(
        &self,
        item_id: Uuid,
        _content: &[u8],
        mime_type: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        self.saved.lock().expect("lock").push((
            item_id,
            mime_type.to_owned(),
            image_type,
            image_index,
        ));
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
    async fn update_to_repository(
        &self,
        item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        self.updates.lock().expect("lock").push(item_id);
        Ok(())
    }
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError> {
        unimplemented!()
    }
    async fn get_metadata_options(
        &self,
        _item_id: Uuid,
    ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
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
    /// Whether the caller satisfies `RequiresElevation` (see [`OkAuth`]).
    elevated: bool,
    /// Whether the caller lacks `EnableUserPreferenceAccess` (see [`OkAuth`]).
    restricted: bool,
    /// Whether the caller is an API key with no user (see [`OkAuth`]).
    userless: bool,
    /// The image processor wired into the state, if any.
    processor: Option<Arc<RecordingProcessor>>,
}

/// An image processor that records each request (its `auto_orient`, and the
/// whole options) and serves the original file, typed `mime_type` and dated
/// `date_modified` when they are set (the image's own date otherwise).
#[derive(Default)]
struct RecordingProcessor {
    auto_orient: Mutex<Vec<bool>>,
    asked: Mutex<Vec<ferrofin_traits::options::ImageProcessingOptions>>,
    mime_type: Option<&'static str>,
    date_modified: Option<chrono::DateTime<chrono::Utc>>,
}

#[async_trait]
impl ferrofin_traits::drawing::ImageProcessor for RecordingProcessor {
    fn supported_input_formats(&self) -> Vec<String> {
        Vec::new()
    }
    fn supports_image_collage_creation(&self) -> bool {
        false
    }
    fn supported_image_output_formats(&self) -> Vec<ferrofin_model::drawing::ImageFormat> {
        Vec::new()
    }
    async fn get_image_dimensions(
        &self,
        _path: &str,
    ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
        unreachable!("not read by the image routes")
    }
    async fn get_item_image_dimensions(
        &self,
        _item_id: Uuid,
        _info: &ItemImageInfo,
    ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
        unreachable!("not read by the image routes")
    }
    async fn get_image_blur_hash(&self, _path: &str) -> Result<String, ServiceError> {
        unreachable!("not read by the image routes")
    }
    async fn get_image_blur_hash_sized(
        &self,
        _path: &str,
        _size: ferrofin_model::drawing::ImageDimensions,
    ) -> Result<String, ServiceError> {
        unreachable!("not read by the image routes")
    }
    async fn get_image_cache_tag(
        &self,
        _item_path: Option<&str>,
        _image: &ItemImageInfo,
    ) -> Result<Option<String>, ServiceError> {
        Ok(None)
    }
    async fn get_image_cache_tag_for_path(
        &self,
        _base_item_path: &str,
        _image_date_modified: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<String>, ServiceError> {
        Ok(None)
    }
    async fn process_image(
        &self,
        options: &ferrofin_traits::options::ImageProcessingOptions,
    ) -> Result<ferrofin_traits::drawing::ProcessedImage, ServiceError> {
        self.auto_orient
            .lock()
            .expect("lock")
            .push(options.auto_orient);
        self.asked.lock().expect("lock").push(options.clone());
        Ok(ferrofin_traits::drawing::ProcessedImage {
            path: options.image.path.clone(),
            mime_type: self.mime_type.map(str::to_owned),
            date_modified: self.date_modified.unwrap_or(options.image.date_modified),
        })
    }
    async fn create_image_collage(
        &self,
        _options: &ferrofin_traits::options::ImageCollageOptions,
        _library_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        unreachable!("not read by the image routes")
    }
}

/// Builds the image stubs, serving `image_path` for item/by-name images and
/// `profile_path` for the user image; write routes record into the stub mutexes.
fn stubs(image_path: String, profile_path: String) -> Stubs {
    Stubs {
        library: Arc::new(StubLibrary {
            image_path,
            swaps: Mutex::new(Vec::new()),
            item_reads: Mutex::new(0),
            photo: false,
            live_tv: false,
        }),
        users: Arc::new(StubUsers {
            profile_path,
            saved: Arc::new(Mutex::new(Vec::new())),
            cleared: Mutex::new(0),
        }),
        providers: Arc::new(StubProviders {
            saved: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            updates: Arc::new(Mutex::new(Vec::new())),
            remote_copy: None,
            remote_asked: Arc::new(Mutex::new(Vec::new())),
        }),
        elevated: false,
        restricted: false,
        userless: false,
        processor: None,
    }
}

/// [`stubs`] for the image *write* routes, which are `RequiresElevation`
/// upstream (`ImageController`'s POST/DELETE actions).
fn elevated_stubs(image_path: String, profile_path: String) -> Stubs {
    Stubs {
        elevated: true,
        ..stubs(image_path, profile_path)
    }
}

/// Builds an [`AppState`] wired with the image stubs.
fn state(s: &Stubs) -> AppState {
    let state = AppState::new(
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
        Arc::new(ferrofin_api::test_support::FakeDto),
        Arc::new(OkAuth {
            elevated: s.elevated,
            restricted: s.restricted,
            userless: s.userless,
        }),
        Arc::new(OkAuth {
            elevated: s.elevated,
            restricted: s.restricted,
            userless: s.userless,
        }),
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
    );
    match &s.processor {
        Some(processor) => state.with_image_processor(processor.clone()),
        None => state,
    }
}

/// Drives one authenticated request through the router, with an optional
/// `(content_type, payload)` body.
/// Like [`send`], but also returns the response headers (for the caching
/// contract assertions) and accepts extra request headers.
async fn send_with_headers(
    s: &Stubs,
    uri: &str,
    extra: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", "Bearer token");
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    let request = builder.body(Body::empty()).expect("request");
    let response = create_router(state(s))
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, bytes.to_vec())
}

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

/// `ImageProcessor.ProcessImage`: a photo's image goes through the processor
/// with `autoOrient`; another item's goes through it without, plain request and
/// resize alike (gl-1118).
#[tokio::test]
async fn a_photos_image_is_processed_with_auto_orientation() {
    let img = TempImage::new(b"JPEGDATA");
    let processor = Arc::new(RecordingProcessor::default());
    let mut s = stubs(img.path(), String::new());
    s.processor = Some(processor.clone());
    let plain = format!("/Items/{ITEM_ID}/Images/Primary");
    let resized = format!("/Items/{ITEM_ID}/Images/Primary?maxWidth=50");

    let (status, body) = send(&s, "GET", &plain, None).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"JPEGDATA"[..])
    );
    assert_eq!(*processor.auto_orient.lock().expect("lock"), [false]);
    send(&s, "GET", &resized, None).await;
    assert_eq!(*processor.auto_orient.lock().expect("lock"), [false, false]);

    let photo = Arc::new(RecordingProcessor::default());
    let s = Stubs {
        library: Arc::new(StubLibrary {
            image_path: img.path(),
            swaps: Mutex::new(Vec::new()),
            item_reads: Mutex::new(0),
            photo: true,
            live_tv: false,
        }),
        processor: Some(photo.clone()),
        ..stubs(img.path(), String::new())
    };
    let (status, body) = send(&s, "GET", &plain, None).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"JPEGDATA"[..])
    );
    send(&s, "GET", &resized, None).await;
    assert_eq!(*photo.auto_orient.lock().expect("lock"), [true, true]);
}

/// gl-1118: `GetImageResult` calls `ProcessImage` on every request, so an
/// item or by-name request naming no transform reaches the processor, a lone
/// `quality` is passed on, and the response takes the processor's type and
/// date (`Age`).
#[rstest::rstest]
#[case::item("/Items/{item}/Images/Primary")]
#[case::indexed("/Items/{item}/Images/Primary/0")]
#[case::by_name("/Genres/Drama/Images/Primary")]
#[tokio::test]
async fn a_plain_image_request_reaches_the_processor(
    #[case] route: &str,
    #[values("", "?quality=50")] query: &str,
) {
    let img = TempImage::new(b"BMPDATA");
    let processor = Arc::new(RecordingProcessor {
        mime_type: Some("image/jpeg"),
        date_modified: Some(chrono::Utc::now() - chrono::Duration::seconds(5000)),
        ..Default::default()
    });
    let mut s = stubs(img.path(), String::new());
    s.processor = Some(processor.clone());
    let uri = format!("{}{query}", route.replace("{item}", &ITEM_ID.to_string()));

    let (status, headers, body) = send_with_headers(&s, &uri, &[]).await;

    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"BMPDATA"[..]),
        "{uri}"
    );
    assert_eq!(headers["content-type"], "image/jpeg", "{uri}");
    let age: i64 = headers["age"].to_str().expect("age").parse().expect("age");
    assert!((5000..5100).contains(&age), "{uri}: age {age}");
    let asked = processor.asked.lock().expect("asked lock");
    let options = asked.first().expect("the image was processed");
    let quality = if query.is_empty() { 100 } else { 50 };
    assert_eq!(options.quality, quality, "{uri}");
}

/// gl-805: `backgroundColor`, `foregroundLayer` and `blur` each reach the
/// processor on their own.
#[rstest::rstest]
#[case::background("backgroundColor=%23FF0000", Some("#FF0000"), None, None)]
#[case::foreground("foregroundLayer=0.6", None, Some("0.6"), None)]
#[case::blur("blur=4", None, None, Some(4))]
#[tokio::test]
async fn compositing_parameters_reach_the_processor(
    #[case] query: &str,
    #[case] background: Option<&str>,
    #[case] foreground: Option<&str>,
    #[case] blur: Option<i32>,
) {
    let img = TempImage::new(b"PNGDATA");
    let processor = Arc::new(RecordingProcessor::default());
    let mut s = stubs(img.path(), String::new());
    s.processor = Some(processor.clone());
    let uri = format!("/Items/{ITEM_ID}/Images/Primary?{query}");

    let (status, _) = send(&s, "GET", &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    let options = processor
        .asked
        .lock()
        .expect("asked lock")
        .first()
        .cloned()
        .expect("the image was processed");
    assert_eq!(options.background_color.as_deref(), background);
    assert_eq!(options.foreground_layer.as_deref(), foreground);
    assert_eq!(options.blur, blur);
}

/// `GetItemImage` declares `percentPlayed` and `unplayedCount`, so a malformed
/// one is `[ApiController]`'s `400`, and a valid one is accepted (its overlay
/// is gl-806).
#[tokio::test]
async fn overlay_parameters_are_bound() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    for (query, expected) in [
        ("percentPlayed=40&unplayedCount=3", StatusCode::OK),
        ("percentPlayed=x", StatusCode::BAD_REQUEST),
        ("unplayedCount=1.5", StatusCode::BAD_REQUEST),
    ] {
        let uri = format!("/Items/{ITEM_ID}/Images/Primary?{query}");
        let (status, _) = send(&s, "GET", &uri, None).await;
        assert_eq!(status, expected, "{query}");
    }
}

/// `GetImageResult` sets the content type and `Last-Modified` from
/// `ProcessImage`'s answer before its `ETag` check, so a `304` carries both.
#[tokio::test]
async fn a_not_modified_image_keeps_its_type_and_date() {
    let img = TempImage::new(b"PNGDATA");
    let date = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .expect("date")
        .with_timezone(&chrono::Utc);
    let processor = Arc::new(RecordingProcessor {
        mime_type: Some("image/webp"),
        date_modified: Some(date),
        ..Default::default()
    });
    let mut s = stubs(img.path(), String::new());
    s.processor = Some(processor);

    let (status, headers, _) = send_with_headers(
        &s,
        &format!("/Items/{ITEM_ID}/Images/Primary?tag=abc"),
        &[("If-None-Match", "\"abc\"")],
    )
    .await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["last-modified"], "Fri, 02 Jan 2026 03:04:05 GMT");
    assert_eq!(headers["transfermode.dlna.org"], "Interactive");
}

/// `GetImageResult` answers `304` when `If-Modified-Since` is at or after
/// `ProcessImage`'s date, before the file is read; earlier, the file's own
/// date decides, as `PhysicalFile` checks it.
#[tokio::test]
async fn if_modified_since_is_checked_against_the_processed_date() {
    let img = TempImage::new(b"PNGDATA");
    let date = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .expect("date")
        .with_timezone(&chrono::Utc);
    let mut s = stubs(img.path(), String::new());
    s.processor = Some(Arc::new(RecordingProcessor {
        mime_type: Some("image/png"),
        date_modified: Some(date),
        ..Default::default()
    }));
    let uri = format!("/Items/{ITEM_ID}/Images/Primary");

    for (since, expected) in [
        ("Fri, 02 Jan 2026 03:04:05 GMT", StatusCode::NOT_MODIFIED),
        ("Fri, 02 Jan 2026 03:04:04 GMT", StatusCode::OK),
    ] {
        let (status, headers, _) =
            send_with_headers(&s, &uri, &[("If-Modified-Since", since)]).await;
        assert_eq!(status, expected, "{since}");
        assert!(headers.contains_key("last-modified"), "{since}");
    }
}

/// Under `Cache-Control: no-cache` upstream skips its own checks, and
/// `PhysicalFile` still answers an `If-Modified-Since` at or after the file's
/// date (not one in the future), and `Age` is still sent. A value other than
/// exactly `no-cache` does not count (all measured on 12.4).
#[tokio::test]
async fn no_cache_skips_the_own_checks_and_keeps_age() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let uri = format!("/Items/{ITEM_ID}/Images/Primary?tag=abc");
    let file_date = httpdate::fmt_http_date(
        std::fs::metadata(img.path())
            .and_then(|m| m.modified())
            .expect("mtime"),
    );

    for (cache_control, since, expected) in [
        ("no-cache", "Fri, 01 Jan 2100 00:00:00 GMT", StatusCode::OK),
        ("no-cache", file_date.as_str(), StatusCode::NOT_MODIFIED),
        (
            "no-cache, max-age=0",
            "Fri, 01 Jan 2100 00:00:00 GMT",
            StatusCode::NOT_MODIFIED,
        ),
    ] {
        let (status, headers, _) = send_with_headers(
            &s,
            &uri,
            &[
                ("Cache-Control", cache_control),
                ("If-Modified-Since", since),
                ("If-None-Match", "\"abc\""),
            ],
        )
        .await;
        assert_eq!(status, expected, "{cache_control} / {since}");
        assert!(headers.contains_key("age"), "{headers:?}");
        let disabled = cache_control == "no-cache";
        assert_eq!(headers.get("etag").is_none(), disabled, "{headers:?}");
    }

    // `StringValues.Contains` looks at every `Cache-Control` line.
    let (status, headers, _) = send_with_headers(
        &s,
        &uri,
        &[
            ("Cache-Control", "max-age=0"),
            ("Cache-Control", "no-cache"),
            ("If-None-Match", "\"abc\""),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{headers:?}");
    assert_eq!(
        headers["cache-control"],
        "no-cache, no-store, must-revalidate"
    );
}

/// `PhysicalFile` answers a stale `If-Unmodified-Since` with `412`, carrying the
/// served file's `Last-Modified` and `GetImageResult`'s headers.
#[tokio::test]
async fn a_stale_if_unmodified_since_is_412_with_the_image_headers() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let file_date = httpdate::fmt_http_date(
        std::fs::metadata(img.path())
            .and_then(|m| m.modified())
            .expect("mtime"),
    );

    let (status, headers, _) = send_with_headers(
        &s,
        &format!("/Items/{ITEM_ID}/Images/Primary"),
        &[("If-Unmodified-Since", "Mon, 01 Jan 2001 00:00:00 GMT")],
    )
    .await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(headers["last-modified"], file_date.as_str());
    assert!(headers.contains_key("age"), "{headers:?}");
    assert_eq!(headers["transfermode.dlna.org"], "Interactive");
}

/// `GetImageResult` answers with `PhysicalFile`, whose range processing is off,
/// so a `Range` gets the whole image and no `Accept-Ranges` is sent (measured on
/// 12.4).
#[tokio::test]
async fn an_image_ignores_a_range() {
    let img = TempImage::new(b"0123456789ABCDEF");
    let s = stubs(img.path(), String::new());

    let (status, headers, body) = send_with_headers(
        &s,
        &format!("/Items/{ITEM_ID}/Images/Primary"),
        &[("Range", "bytes=0-3")],
    )
    .await;

    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"0123456789ABCDEF"[..])
    );
    assert!(headers.get("accept-ranges").is_none(), "{headers:?}");
    assert!(headers.get("content-range").is_none(), "{headers:?}");
}

/// `BaseItem.GetImageInfo` is `GetImages(type).ElementAtOrDefault(index)`, so
/// a negative index finds no image: a `404`, not the first image.
#[tokio::test]
async fn a_negative_image_index_is_not_found() {
    let img = TempImage::new(b"IDX0");
    let s = stubs(img.path(), String::new());
    for uri in [
        format!("/Items/{ITEM_ID}/Images/Primary/-1"),
        format!("/Items/{ITEM_ID}/Images/Primary?imageIndex=-1"),
    ] {
        let (status, _) = send(&s, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

/// Only the actions with no index in their route declare `[FromQuery] int?
/// imageIndex`: there a malformed one is a `400`, and the indexed routes, which
/// do not bind it, ignore it.
#[tokio::test]
async fn only_the_unindexed_routes_bind_a_query_image_index() {
    let img = TempImage::new(b"IDX0");
    let s = stubs(img.path(), String::new());
    for (uri, expected) in [
        (
            format!("/Items/{ITEM_ID}/Images/Primary?imageIndex=x"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("/Items/{ITEM_ID}/Images/Primary?imageIndex="),
            StatusCode::OK,
        ),
        // The binder's hex forms, measured on 12.4.
        (
            format!("/Items/{ITEM_ID}/Images/Primary?imageIndex=0x0"),
            StatusCode::OK,
        ),
        (
            format!("/Items/{ITEM_ID}/Images/Primary/0?imageIndex=x"),
            StatusCode::OK,
        ),
        (
            "/Genres/Drama/Images/Primary?imageIndex=x".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/Genres/Drama/Images/Primary/0?imageIndex=x".to_owned(),
            StatusCode::OK,
        ),
    ] {
        let (status, _) = send(&s, "GET", &uri, None).await;
        assert_eq!(status, expected, "{uri}");
    }
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
    assert!(body.is_empty(), "{body:?}");
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

// A chapter thumbnail is not an item image row — it lives on the chapter
// itself, so serving one has to resolve through the chapter seam (upstream
// `BaseItem.GetImageInfo(ImageType.Chapter, index)`). Without this the player's
// chapter thumbnails 404 however well the extraction ran.
#[tokio::test]
async fn chapter_image_serves_from_the_chapter_seam() {
    let img = TempImage::new(b"CHAPTERFRAME");
    let s = stubs(img.path(), String::new());
    let (status, body) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Chapter/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"CHAPTERFRAME");

    // An index past the last chapter is still a 404.
    let (status, _) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Chapter/7"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
async fn authenticated_image_aliases_resolve_the_item_before_serving() {
    // An existing image row proves existence, but does not establish visibility.
    // Every authenticated alias must resolve the item before serving bytes.
    let img = TempImage::new(b"IMAGEBYTES");
    let s = stubs(img.path(), String::new());

    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images/Primary"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"IMAGEBYTES");
    assert_eq!(
        s.library.item_reads(),
        1,
        "served image must check visibility"
    );

    // Same for the indexed and fully-parametrized aliases.
    let (status, _) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Primary/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Primary/0/tag/0/0/0/0/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        s.library.item_reads(),
        3,
        "each alias must check visibility"
    );
}

#[tokio::test]
async fn item_image_404_still_tells_missing_item_from_missing_image() {
    // Missing items and missing images retain their respective responses.
    let img = TempImage::new(b"X");
    let s = stubs(img.path(), String::new());

    let missing = Uuid::from_u128(0xDEAD);

    // All three item-image route shapes must keep the
    // distinction — the bare route, the indexed alias, and the fully
    // parametrized alias. Flipping either alias to `ItemResolved::Yes` changes
    // its missing-item body from "item {id}" to "item {id} has no Primary image
    // at 0", and with only the bare route asserted the whole suite stayed green.
    for (label, path) in [
        ("bare", format!("/Items/{missing}/Images/Primary")),
        ("indexed", format!("/Items/{missing}/Images/Primary/0")),
        (
            "parametrized",
            // {imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}
            format!("/Items/{missing}/Images/Primary/0/tag/jpg/100/100/0/0"),
        ),
    ] {
        let before = s.library.item_reads();
        let (status, body) = send(&s, "GET", &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}: {path}");
        let text = String::from_utf8_lossy(&body).into_owned();
        assert!(
            text.contains(&format!("item {missing}")) && !text.contains("has no"),
            "{label}: missing item should 404 as the item, got {text}"
        );
        assert_eq!(
            s.library.item_reads(),
            before + 1,
            "{label}: the miss path must read the item exactly once"
        );
    }

    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images/Logo"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(
        text.contains("has no Logo image at 0"),
        "existing item without that image should 404 as the image, got {text}"
    );
}

#[tokio::test]
async fn by_name_image_miss_does_not_re_read_the_item() {
    // The by-name routes resolved the item row themselves, so an image miss
    // must not go back to the database for it — and its 404 still names the
    // image, not the item.
    let img = TempImage::new(b"X");
    let s = stubs(img.path(), String::new());

    let (status, body) = send(&s, "GET", "/Genres/Drama/Images/Logo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(
        text.contains("has no Logo image at 0"),
        "by-name image miss should 404 as the image, got {text}"
    );
    assert_eq!(
        s.library.item_reads(),
        0,
        "by-name miss re-read the item row"
    );
}

/// `ImageController`: a remote row is served from its local copy
/// (`ConvertImageToLocal`), fetched on request.
#[tokio::test]
async fn remote_image_backdrop_is_served_from_its_local_copy() {
    let img = TempImage::new(b"X");
    let copy = TempImage::new(b"REMOTE");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let s = Stubs {
        providers: Arc::new(StubProviders {
            saved: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            updates: Arc::new(Mutex::new(Vec::new())),
            remote_copy: Some(copy.path()),
            remote_asked: Arc::clone(&asked),
        }),
        ..stubs(img.path(), String::new())
    };
    let (status, body) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Backdrop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"REMOTE");
    assert_eq!(
        *asked.lock().expect("lock"),
        ["https://remote.example/backdrop.jpg"]
    );
}

/// A Live TV item's row outside the metadata directory is not served and not
/// listed, though the file exists and another item's row may point at it; a URL
/// is served from its copy as on any item. `FakeConfig` has no metadata
/// directory, so here every local row is outside it.
#[tokio::test]
async fn a_live_tv_items_local_row_is_not_served_outside_the_metadata_directory() {
    let img = TempImage::new(b"X");
    let copy = TempImage::new(b"REMOTE");
    let s = Stubs {
        library: Arc::new(StubLibrary {
            image_path: img.path(),
            swaps: Mutex::new(Vec::new()),
            item_reads: Mutex::new(0),
            photo: false,
            live_tv: true,
        }),
        providers: Arc::new(StubProviders {
            saved: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            updates: Arc::new(Mutex::new(Vec::new())),
            remote_copy: Some(copy.path()),
            remote_asked: Arc::new(Mutex::new(Vec::new())),
        }),
        ..stubs(img.path(), String::new())
    };
    for method in ["GET", "HEAD"] {
        let (status, body) = send(
            &s,
            method,
            &format!("/Items/{ITEM_ID}/Images/Primary"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        assert_ne!(body, b"X", "{method}");
    }
    let (status, body) = send(
        &s,
        "GET",
        &format!("/Items/{ITEM_ID}/Images/Backdrop"),
        None,
    )
    .await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"REMOTE"[..]));

    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/Images"), None).await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(listed.as_array().expect("infos").len(), 1, "{listed}");
    assert_eq!(listed[0]["ImageType"], "Backdrop");
}

#[tokio::test]
async fn remote_image_backdrop_is_404_without_a_local_copy() {
    let img = TempImage::new(b"X");
    let s = stubs(img.path(), String::new());
    // The Backdrop image is a remote URL that cannot be fetched → 404.
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
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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
    assert_eq!(
        saved.as_slice(),
        [(ITEM_ID, "image/png".to_owned(), ImageType::Primary, None)]
    );
}

#[tokio::test]
async fn indexed_upload_ignores_every_bound_index_for_single_and_multiple_images() {
    let s = elevated_stubs(String::new(), String::new());
    let mut expected = Vec::new();
    for (kind, image_type) in [
        ("Backdrop", ImageType::Backdrop),
        ("Primary", ImageType::Primary),
    ] {
        for index in [i32::MIN, -1, 0, 1, 2, i32::MAX] {
            let (status, _) = send(
                &s,
                "POST",
                &format!("/Items/{ITEM_ID}/Images/{kind}/{index}"),
                Some(("image/png", "aGk=")),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{kind}/{index}");
            expected.push((ITEM_ID, "image/png".to_owned(), image_type, None));
        }
    }
    assert_eq!(*s.providers.saved.lock().expect("lock"), expected);
}

#[tokio::test]
async fn indexed_upload_rejects_unbound_indices_without_saving() {
    let s = elevated_stubs(String::new(), String::new());
    for index in ["2147483648", "-2147483649", "invalid"] {
        let (status, _) = send(
            &s,
            "POST",
            &format!("/Items/{ITEM_ID}/Images/Backdrop/{index}"),
            Some(("image/png", "aGk=")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert!(s.providers.saved.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn set_item_image_bad_content_type_is_400() {
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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

/// `DeleteItemImage` binds `imageIndex`, so a malformed one is the binder's
/// `400`, which comes before the item is read: a missing item is a `400` too.
#[tokio::test]
async fn delete_item_image_with_a_malformed_index_is_400() {
    let s = elevated_stubs(String::new(), String::new());
    for item in [ITEM_ID, MISSING_ID] {
        let uri = format!("/Items/{item}/Images/Primary?imageIndex=x");
        let (status, _) = send(&s, "DELETE", &uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
    assert!(s.providers.deleted.lock().expect("lock").is_empty());
}

/// gl-1119: `DeleteItemImage` declares one query member, `imageIndex`, so a
/// malformed size, quality or overlay value is never read, and a missing item
/// is then the `404`. Measured on 12.4.
#[rstest::rstest]
#[case::max_width(ITEM_ID, "maxWidth=x", StatusCode::NO_CONTENT, Some(0))]
#[case::quality(ITEM_ID, "quality=x", StatusCode::NO_CONTENT, Some(0))]
#[case::percent_played(ITEM_ID, "percentPlayed=x", StatusCode::NO_CONTENT, Some(0))]
#[case::unplayed_count(ITEM_ID, "unplayedCount=x", StatusCode::NO_CONTENT, Some(0))]
#[case::blank_image_index(ITEM_ID, "imageIndex=", StatusCode::NO_CONTENT, Some(0))]
#[case::hex_image_index(ITEM_ID, "imageIndex=0x1", StatusCode::NO_CONTENT, Some(1))]
#[case::missing_item(MISSING_ID, "maxWidth=x", StatusCode::NOT_FOUND, None)]
#[tokio::test]
async fn delete_item_image_binds_only_its_image_index(
    #[case] item: Uuid,
    #[case] query: &str,
    #[case] expected: StatusCode,
    #[case] deleted_index: Option<i32>,
) {
    let s = elevated_stubs(String::new(), String::new());
    let uri = format!("/Items/{item}/Images/Primary?{query}");
    let (status, _) = send(&s, "DELETE", &uri, None).await;
    assert_eq!(status, expected, "{uri}");
    let deleted = s.providers.deleted.lock().expect("lock").clone();
    let expected_deleted: Vec<_> = deleted_index.map(|i| (ITEM_ID, i)).into_iter().collect();
    assert_eq!(deleted, expected_deleted, "{uri}");
}

#[tokio::test]
async fn delete_item_image_missing_item_is_404() {
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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
async fn update_item_image_index_saves_the_item_only_after_a_real_swap() {
    let s = elevated_stubs(String::new(), String::new());
    for (new_index, expected_saves) in [(3, 1), (99, 1)] {
        let (status, _) = send(
            &s,
            "POST",
            &format!("/Items/{ITEM_ID}/Images/Backdrop/1/Index?newIndex={new_index}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            s.providers.updates.lock().expect("lock").len(),
            expected_saves
        );
    }
    assert_eq!(*s.providers.updates.lock().expect("lock"), [ITEM_ID]);
}

#[tokio::test]
async fn update_item_image_index_non_multiple_type_is_400() {
    let s = elevated_stubs(String::new(), String::new());
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
    let s = elevated_stubs(String::new(), String::new());
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
    let config: ferrofin_model::dto::ConfigImageTypes =
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
        [(
            USER_ID.to_string(),
            "image/png".to_owned(),
            ".png".to_owned()
        )]
    );
}

/// `AssertCanUpdateUser(…, restrictUserPreferences: true)`: a non-admin without
/// `EnableUserPreferenceAccess` cannot change even their own profile image.
#[tokio::test]
async fn user_image_writes_require_preference_access() {
    let s = Stubs {
        restricted: true,
        ..stubs(String::new(), String::new())
    };
    let (status, _) = send(&s, "POST", "/UserImage", Some(("image/png", "aGk="))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&s, "DELETE", "/UserImage", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(s.users.saved.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn post_user_image_bad_content_type_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "POST", "/UserImage", Some(("text/plain", "aGk="))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Jellyfin 12 maps the non-standard `image/jpg` to `.jpg`
/// (`MimeTypes._extensionLookup`), so the avatar is `profile.jpg`, never an
/// extension-less `profile.`.
#[tokio::test]
async fn post_user_image_maps_image_jpg_to_jpg() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "POST", "/UserImage", Some(("image/jpg", "aGk="))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.users.saved.lock().expect("lock").as_slice(),
        [(
            USER_ID.to_string(),
            "image/jpg".to_owned(),
            ".jpg".to_owned()
        )]
    );
}

/// `MimeTypes.ToExtension` falls back to the `MimeTypes` package's base table,
/// so image types outside the override table upload on the user and item
/// routes, stored under their own extension (upstream `ImageControllerTests`
/// expects `image/apng` and `image/avif` to be accepted).
#[rstest::rstest]
#[case("image/avif", ".avif")]
#[case("image/apng", ".apng")]
#[case("image/heic", ".heic")]
#[tokio::test]
async fn image_uploads_accept_base_table_image_types(
    #[case] content_type: &str,
    #[case] extension: &str,
) {
    let s = elevated_stubs(String::new(), String::new());
    let (status, _) = send(&s, "POST", "/UserImage", Some((content_type, "aGk="))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.users.saved.lock().expect("lock").as_slice(),
        [(
            USER_ID.to_string(),
            content_type.to_owned(),
            extension.to_owned()
        )]
    );

    let (status, _) = send(
        &s,
        "POST",
        &format!("/Items/{ITEM_ID}/Images/Primary"),
        Some((content_type, "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.providers.saved.lock().expect("lock").as_slice(),
        [(ITEM_ID, content_type.to_owned(), ImageType::Primary, None)]
    );
}

/// Upstream `TryGetImageExtensionFromContentType` fails when `ToExtension`
/// knows no extension for the type, so an unknown image subtype is
/// `400 Incorrect ContentType.` on every user and item upload route, and
/// nothing is saved.
#[rstest::rstest]
#[case::user_image("/UserImage")]
#[case::legacy_user_image("/Users/{user}/Images/Primary")]
#[case::indexed_legacy_user_image("/Users/{user}/Images/Primary/0")]
#[case::item_image("/Items/{item}/Images/Primary")]
#[case::item_image_by_index("/Items/{item}/Images/Primary/0")]
#[tokio::test]
async fn image_uploads_reject_unknown_image_subtypes(#[case] route: &str) {
    let s = elevated_stubs(String::new(), String::new());
    let uri = route
        .replace("{user}", &USER_ID.to_string())
        .replace("{item}", &ITEM_ID.to_string());
    let (status, body) = send(&s, "POST", &uri, Some(("image/x-unknown", "aGk="))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    assert!(
        String::from_utf8_lossy(&body).contains("Incorrect ContentType."),
        "{uri}"
    );
    assert!(s.users.saved.lock().expect("lock").is_empty(), "{uri}");
    assert!(s.providers.saved.lock().expect("lock").is_empty(), "{uri}");
}

/// gl-24: Jellyfin sets no `MaxRequestBodySize`, so Kestrel's 30,000,000-byte
/// default is the only cap on an image upload. A 4 MB base64 body (a 3 MB
/// image) saves on every user and item upload route instead of tripping axum's
/// implicit 2 MiB limit, and a body past 30,000,000 bytes is a `413` that saves
/// nothing.
#[rstest::rstest]
#[case::user_image("/UserImage")]
#[case::legacy_user_image("/Users/{user}/Images/Primary")]
#[case::indexed_legacy_user_image("/Users/{user}/Images/Primary/0")]
#[case::item_image("/Items/{item}/Images/Primary")]
#[case::item_image_by_index("/Items/{item}/Images/Primary/0")]
#[tokio::test]
async fn image_uploads_take_bodies_up_to_kestrels_default_limit(#[case] route: &str) {
    let s = elevated_stubs(String::new(), String::new());
    let uri = route
        .replace("{user}", &USER_ID.to_string())
        .replace("{item}", &ITEM_ID.to_string());
    let (status, _) = send(
        &s,
        "POST",
        &uri,
        Some(("image/png", &"A".repeat(4_000_000))),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{uri}");
    let saved = || {
        s.users.saved.lock().expect("lock").len() + s.providers.saved.lock().expect("lock").len()
    };
    assert_eq!(saved(), 1, "{uri}");

    let (status, _) = send(
        &s,
        "POST",
        &uri,
        Some(("image/png", &"A".repeat(30_000_004))),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
    assert_eq!(saved(), 1, "{uri}");
}

/// `=` before the end of the body is invalid data to `FromBase64Transform`, so
/// the upload is refused and nothing is saved (gl-689).
#[tokio::test]
async fn post_user_image_with_padding_inside_the_data_is_400() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(&s, "POST", "/UserImage", Some(("image/png", "aG=k"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(s.users.saved.lock().expect("lock").is_empty());
}

/// `PostUserImage`/`DeleteUserImage` (and the legacy forms forwarding to them):
/// `GetUserById` finds no account for the id, so the action returns `NotFound()`
/// before anything is written (gl-685).
#[rstest::rstest]
#[case::post_user_image("POST", format!("/UserImage?userId={MISSING_ID}"))]
#[case::delete_user_image("DELETE", format!("/UserImage?userId={MISSING_ID}"))]
#[case::post_legacy("POST", format!("/Users/{MISSING_ID}/Images/Primary"))]
#[case::delete_legacy("DELETE", format!("/Users/{MISSING_ID}/Images/Primary"))]
#[case::post_indexed_legacy("POST", format!("/Users/{MISSING_ID}/Images/Primary/0"))]
#[case::delete_indexed_legacy("DELETE", format!("/Users/{MISSING_ID}/Images/Primary/0"))]
#[tokio::test]
async fn user_image_writes_for_an_unknown_user_are_404(#[case] method: &str, #[case] uri: String) {
    // Elevated, so the administrator gate of `GetUserId` lets the other id through.
    let s = elevated_stubs(String::new(), String::new());
    let (status, _) = send(&s, method, &uri, Some(("image/png", "aGk="))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(s.users.saved.lock().expect("lock").is_empty());
}

/// `GetUserId`'s administrator gate runs before the lookup: a caller who is not
/// an administrator naming another (here unknown) user is a `403`, not a `404`.
#[rstest::rstest]
#[case::post("POST")]
#[case::delete("DELETE")]
#[tokio::test]
async fn user_image_writes_for_another_user_are_403_before_the_lookup(#[case] method: &str) {
    let s = stubs(String::new(), String::new());
    let uri = format!("/UserImage?userId={MISSING_ID}");
    let (status, _) = send(&s, method, &uri, Some(("image/png", "aGk="))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(s.users.saved.lock().expect("lock").is_empty());
}

/// An all-zero `userId` is `IsNullOrEmpty` to `GetUserId`, so it falls back to
/// the caller.
#[tokio::test]
async fn post_user_image_with_an_empty_user_id_targets_the_caller() {
    let s = stubs(String::new(), String::new());
    let uri = format!("/UserImage?userId={}", Uuid::nil());
    let (status, _) = send(&s, "POST", &uri, Some(("image/png", "aGk="))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.users.saved.lock().expect("lock").as_slice(),
        [(
            USER_ID.to_string(),
            "image/png".to_owned(),
            ".png".to_owned()
        )]
    );
}

/// An API key with no user has a nil effective id, and `GetUserById(Guid.Empty)`
/// throws `ArgumentException`, which the exception middleware answers `400`.
#[rstest::rstest]
#[case::post("POST")]
#[case::delete("DELETE")]
#[tokio::test]
async fn user_image_writes_without_a_user_are_400(#[case] method: &str) {
    let s = Stubs {
        userless: true,
        ..stubs(String::new(), String::new())
    };
    let (status, _) = send(&s, method, "/UserImage", Some(("image/png", "aGk="))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(s.users.saved.lock().expect("lock").is_empty());
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

// ── the image caching contract (C# GetImageResult; poster pop-in fix) ───────

#[tokio::test]
async fn tagged_image_serves_the_immutable_caching_contract() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let (status, headers, body) = send_with_headers(
        &s,
        &format!("/Items/{ITEM_ID}/Images/Primary?tag=abc123"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"PNGDATA");
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable"),
        "a tagged (content-addressed) URL is immutable for a year"
    );
    assert_eq!(
        headers.get("etag").and_then(|v| v.to_str().ok()),
        Some("\"abc123\""),
        "the tag doubles as the ETag"
    );
    assert!(headers.contains_key("age"));
    assert_eq!(
        headers.get("vary").and_then(|v| v.to_str().ok()),
        Some("Accept")
    );
}

#[tokio::test]
async fn matching_if_none_match_is_an_empty_304() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let (status, headers, body) = send_with_headers(
        &s,
        &format!("/Items/{ITEM_ID}/Images/Primary?tag=abc123"),
        &[("If-None-Match", "\"abc123\"")],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "304 carries no body");
    assert_eq!(
        headers.get("etag").and_then(|v| v.to_str().ok()),
        Some("\"abc123\""),
        "304 revalidates the same ETag"
    );
}

#[tokio::test]
async fn untagged_image_is_public_and_client_no_cache_is_mirrored() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let (status, headers, _) =
        send_with_headers(&s, &format!("/Items/{ITEM_ID}/Images/Primary"), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("public"),
        "no tag => cacheable but revalidated"
    );
    assert!(headers.get("etag").is_none());

    let (status, headers, _) = send_with_headers(
        &s,
        &format!("/Items/{ITEM_ID}/Images/Primary?tag=abc123"),
        &[("Cache-Control", "no-cache")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-cache, no-store, must-revalidate"),
        "client no-cache disables caching, as upstream does"
    );
}

#[tokio::test]
async fn file_metadata_validators_do_not_override_image_tags() {
    let img = TempImage::new(b"PNGDATA");
    let s = stubs(img.path(), String::new());
    let path = format!("/Items/{ITEM_ID}/Images/Primary?tag=abc123");
    for condition in ["If-Match", "If-None-Match"] {
        let (status, headers, body) = send_with_headers(
            &s,
            &path,
            &[("Cache-Control", "no-cache"), (condition, "*")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"PNGDATA");
        assert!(headers.get("etag").is_none());
    }
}

// The legacy `/Users/{userId}/Images/{imageType}[/{index}]` forms (upstream
// [Obsolete] + hidden from OpenAPI; jellyfin-web's apiclient still requests
// avatars this way) forward to the /UserImage handlers — the path's image
// type/index are accepted and ignored, as users carry one profile image.

#[tokio::test]
async fn legacy_user_image_routes_forward() {
    let img = TempImage::new(b"PROFILE");
    let s = stubs(String::new(), img.path());
    let (status, body) = send(&s, "GET", &format!("/Users/{USER_ID}/Images/Primary"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"PROFILE");

    // The indexed form serves the same single profile image.
    let (status, body) = send(
        &s,
        "GET",
        &format!("/Users/{USER_ID}/Images/Primary/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"PROFILE");

    // HEAD is registered on both forms: same status, no body.
    let (status, body) = send(
        &s,
        "HEAD",
        &format!("/Users/{USER_ID}/Images/Primary"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty(), "{body:?}");
    let (status, body) = send(
        &s,
        "HEAD",
        &format!("/Users/{USER_ID}/Images/Primary/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty(), "{body:?}");
}

#[tokio::test]
async fn legacy_user_image_upload_and_delete_forward() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Users/{USER_ID}/Images/Primary"),
        Some(("image/png", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Recorded exactly as the modern /UserImage POST would record it.
    assert_eq!(
        s.users.saved.lock().expect("lock").as_slice(),
        [(
            USER_ID.to_string(),
            "image/png".to_owned(),
            ".png".to_owned()
        )]
    );

    let (status, _) = send(
        &s,
        "DELETE",
        &format!("/Users/{USER_ID}/Images/Primary"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// `PostUserImageByIndexLegacy`/`DeleteUserImageByIndexLegacy`: the indexed form
/// forwards to `PostUserImage`/`DeleteUserImage` and ignores the image type and
/// index (gl-686).
#[tokio::test]
async fn indexed_legacy_user_image_upload_and_delete_forward() {
    let s = stubs(String::new(), String::new());
    let (status, _) = send(
        &s,
        "POST",
        &format!("/Users/{USER_ID}/Images/Backdrop/7"),
        Some(("image/png", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.users.saved.lock().expect("lock").as_slice(),
        [(
            USER_ID.to_string(),
            "image/png".to_owned(),
            ".png".to_owned()
        )]
    );

    let (status, _) = send(
        &s,
        "DELETE",
        &format!("/Users/{USER_ID}/Images/Primary/0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Upstream binds the indexed form's `{index}` as an `int`: any `int` is
/// accepted (and ignored), and an index that is not one fails model binding,
/// which `[ApiController]` answers `400`.
#[rstest::rstest]
#[case::post_negative("POST", "-1", StatusCode::NO_CONTENT)]
#[case::delete_negative("DELETE", "-1", StatusCode::NO_CONTENT)]
#[case::post_not_a_number("POST", "abc", StatusCode::BAD_REQUEST)]
#[case::delete_not_a_number("DELETE", "abc", StatusCode::BAD_REQUEST)]
#[case::post_past_int_max("POST", "2147483648", StatusCode::BAD_REQUEST)]
#[case::delete_past_int_max("DELETE", "2147483648", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn indexed_legacy_user_image_writes_bind_the_index_as_an_int(
    #[case] method: &str,
    #[case] index: &str,
    #[case] expected: StatusCode,
) {
    let s = stubs(String::new(), String::new());
    let uri = format!("/Users/{USER_ID}/Images/Primary/{index}");
    let (status, _) = send(&s, method, &uri, Some(("image/png", "aGk="))).await;
    assert_eq!(status, expected);
    let saved = s.users.saved.lock().expect("lock").len();
    let uploaded = method == "POST" && expected == StatusCode::NO_CONTENT;
    assert_eq!(saved, usize::from(uploaded));
}

/// Upstream binds the legacy routes' `{imageType}` as `[FromRoute, Required]
/// ImageType` (gl-691), through `EnumTypeModelBinder` like the route
/// `{format}`: a member name in any case, a defined number, or a comma list
/// (ORed) binds and is then ignored; anything else is a `400` before the
/// action runs, so nothing is served, saved or cleared.
#[rstest::rstest]
#[case::name("Primary", true)]
#[case::lowercase("primary", true)]
#[case::other_member("Backdrop", true)]
#[case::number("0", true)]
#[case::last_member("12", true)]
#[case::comma_list("Primary,Art", true)]
#[case::undefined_number("13", false)]
#[case::negative("-1", false)]
#[case::not_a_type("NotAType", false)]
#[tokio::test]
async fn legacy_user_image_routes_bind_the_image_type(
    #[case] image_type: &str,
    #[case] binds: bool,
) {
    let img = TempImage::new(b"PROFILE");
    let s = stubs(String::new(), img.path());
    let read = if binds {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let write = if binds {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::BAD_REQUEST
    };
    for form in [
        format!("/Users/{USER_ID}/Images/{image_type}"),
        format!("/Users/{USER_ID}/Images/{image_type}/0"),
    ] {
        assert_eq!(send(&s, "GET", &form, None).await.0, read, "GET {form}");
        assert_eq!(send(&s, "HEAD", &form, None).await.0, read, "HEAD {form}");
        assert_eq!(
            send(&s, "POST", &form, Some(("image/png", "aGk="))).await.0,
            write,
            "POST {form}"
        );
        assert_eq!(
            send(&s, "DELETE", &form, None).await.0,
            write,
            "DELETE {form}"
        );
    }
    let writes = if binds { 2 } else { 0 };
    assert_eq!(s.users.saved.lock().expect("lock").len(), writes);
    assert_eq!(*s.users.cleared.lock().expect("lock"), writes);
}

/// The item image routes bind `{imageType}` the same way: a defined number
/// serves, an undefined one is a `400`. `Primary,Art` ORs to `Art` (0 | 1),
/// which this item has none of: a `404`, past binding.
#[rstest::rstest]
#[case::number("0", StatusCode::OK)]
#[case::comma_list("Primary,Art", StatusCode::NOT_FOUND)]
#[case::undefined_number("13", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn item_image_routes_bind_a_numeric_image_type(
    #[case] image_type: &str,
    #[case] expected: StatusCode,
) {
    let img = TempImage::new(b"POSTER");
    let s = stubs(img.path(), String::new());
    let uri = format!("/Items/{ITEM_ID}/Images/{image_type}");
    assert_eq!(send(&s, "GET", &uri, None).await.0, expected, "{uri}");
}

#[tokio::test]
async fn profile_images_are_public_on_all_read_routes() {
    let img = TempImage::new(b"PROFILE");
    let s = stubs(String::new(), img.path());
    for method in ["GET", "HEAD"] {
        for path in [
            format!("/UserImage?userId={USER_ID}"),
            format!("/Users/{USER_ID}/Images/Primary"),
            format!("/Users/{USER_ID}/Images/Primary/0"),
        ] {
            let app = state(&s);
            let response = create_router(app)
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                body.as_ref(),
                if method == "HEAD" {
                    b"".as_slice()
                } else {
                    b"PROFILE".as_slice()
                }
            );
        }
    }
}

#[tokio::test]
async fn anonymous_profile_image_requires_a_nonempty_user_id() {
    let s = stubs(String::new(), String::new());
    for path in [
        "/UserImage".to_owned(),
        format!("/UserImage?userId={}", Uuid::nil()),
    ] {
        let app = state(&s);
        let response = create_router(app)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

/// `GetUserImage` passes `GetImageInternal` no sizes and a quality of 90, and the
/// legacy forms drop what they declare, so nothing in the URL sizes an avatar.
/// `GetImageResult` calls `ProcessImage` on every request, which re-encodes a
/// source format the client's output formats do not list, so a plain request
/// reaches the processor too (gl-1114).
#[rstest::rstest]
#[tokio::test]
async fn user_images_reach_the_processor_with_upstreams_fixed_options(
    #[values(
        "/UserImage",
        "/Users/{user}/Images/Primary",
        "/Users/{user}/Images/Primary/0"
    )]
    route: &str,
    #[values(
        "",
        "?width=40&height=30&maxWidth=20&maxHeight=10&fillWidth=50&fillHeight=60&blur=2&quality=60"
    )]
    query: &str,
) {
    let img = TempImage::new(b"PROFILE");
    let processor = Arc::new(RecordingProcessor::default());
    let mut s = stubs(String::new(), img.path());
    s.processor = Some(processor.clone());
    let uri = format!("{}{query}", route.replace("{user}", &USER_ID.to_string()));

    let (status, body) = send(&s, "GET", &uri, None).await;

    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"PROFILE"[..]),
        "{uri}"
    );
    let asked = processor.asked.lock().expect("asked lock");
    let options = asked.first().expect("the image was processed");
    assert_eq!(options.quality, 90, "{uri}");
    assert_eq!(
        (
            options.width,
            options.height,
            options.max_width,
            options.max_height
        ),
        (None, None, None, None),
        "{uri}"
    );
    assert_eq!(
        (options.fill_width, options.fill_height, options.blur),
        (None, None, None),
        "{uri}"
    );
}

/// `tag` and `format` are what all three routes pass on to `GetImageInternal`.
#[rstest::rstest]
#[case::user_image("/UserImage")]
#[case::legacy("/Users/{user}/Images/Primary")]
#[case::indexed_legacy("/Users/{user}/Images/Primary/0")]
#[tokio::test]
async fn user_images_keep_their_tag_and_format(#[case] route: &str) {
    let img = TempImage::new(b"PROFILE");
    let processor = Arc::new(RecordingProcessor::default());
    let mut s = stubs(String::new(), img.path());
    s.processor = Some(processor.clone());
    let uri = format!(
        "{}?tag=abc&format=jpg",
        route.replace("{user}", &USER_ID.to_string())
    );

    let (status, headers, _) = send_with_headers(&s, &uri, &[]).await;

    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["etag"], "\"abc\"", "{uri}");
    assert_eq!(
        headers["cache-control"], "public, max-age=31536000, immutable",
        "{uri}"
    );
    let asked = processor.asked.lock().expect("asked lock");
    assert_eq!(
        asked
            .first()
            .expect("the image was processed")
            .supported_output_formats,
        [ferrofin_model::drawing::ImageFormat::Jpg],
        "{uri}"
    );
}

/// `GetUserImage` binds `userId`, `tag` and `format` and nothing else, so a
/// malformed size is never seen, while the legacy forms take `userId` from the
/// route and `imageIndex` is a query member only on the form with no index in
/// its route. `format` is a nullable enum: an unparseable one binds null, and an
/// undefined number throws once `ProcessImage` reads it, so a user with no image
/// is still a `404`. Each row was measured on Jellyfin 12.4 (gl-1114).
#[rstest::rstest]
#[case::size_is_not_bound("/UserImage?width=x", StatusCode::OK)]
#[case::quality_is_not_bound("/UserImage?quality=x", StatusCode::OK)]
#[case::blur_is_not_bound("/UserImage?blur=x", StatusCode::OK)]
#[case::empty_size("/UserImage?width=", StatusCode::OK)]
#[case::user_id_is_bound("/UserImage?userId=junk", StatusCode::BAD_REQUEST)]
#[case::unparseable_format("/UserImage?format=bogus", StatusCode::OK)]
#[case::undefined_format("/UserImage?format=6", StatusCode::BAD_REQUEST)]
#[case::undefined_format_with_no_image(
    "/UserImage?userId={missing}&format=6",
    StatusCode::NOT_FOUND
)]
#[case::legacy_image_index("/Users/{user}/Images/Primary?imageIndex=x", StatusCode::BAD_REQUEST)]
#[case::legacy_user_id_is_the_routes("/Users/{user}/Images/Primary?userId=junk", StatusCode::OK)]
#[case::legacy_background_color_is_a_string(
    "/Users/{user}/Images/Primary?backgroundColor=zz",
    StatusCode::OK
)]
#[case::legacy_undefined_format("/Users/{user}/Images/Primary?format=6", StatusCode::BAD_REQUEST)]
#[case::indexed_image_index_is_the_routes(
    "/Users/{user}/Images/Primary/0?imageIndex=x",
    StatusCode::OK
)]
#[case::indexed_user_id_is_the_routes("/Users/{user}/Images/Primary/0?userId=junk", StatusCode::OK)]
#[tokio::test]
async fn user_image_routes_bind_what_their_actions_declare(
    #[case] route: &str,
    #[case] expected: StatusCode,
) {
    let img = TempImage::new(b"PROFILE");
    let mut s = stubs(String::new(), img.path());
    s.processor = Some(Arc::new(RecordingProcessor::default()));
    let uri = route
        .replace("{user}", &USER_ID.to_string())
        .replace("{missing}", &MISSING_ID.to_string());
    let (status, _) = send(&s, "GET", &uri, None).await;
    assert_eq!(status, expected, "{uri}");
}

/// The legacy forms declare these numbers and drop them, and ASP.NET binds what
/// is declared: a malformed one is `[ApiController]`'s `400`, and an empty or a
/// valid one has no effect. Measured on Jellyfin 12.4 (gl-1114).
#[rstest::rstest]
#[tokio::test]
async fn legacy_user_image_forms_bind_every_number_they_declare(
    #[values("/Users/{user}/Images/Primary", "/Users/{user}/Images/Primary/0")] route: &str,
    #[values(
        "width",
        "height",
        "maxWidth",
        "maxHeight",
        "fillWidth",
        "fillHeight",
        "quality",
        "blur",
        "percentPlayed",
        "unplayedCount"
    )]
    member: &str,
) {
    let img = TempImage::new(b"PROFILE");
    let s = stubs(String::new(), img.path());
    let route = route.replace("{user}", &USER_ID.to_string());
    for (value, expected) in [
        ("x", StatusCode::BAD_REQUEST),
        ("12", StatusCode::OK),
        ("", StatusCode::OK),
    ] {
        let uri = format!("{route}?{member}={value}");
        assert_eq!(send(&s, "GET", &uri, None).await.0, expected, "{uri}");
    }
}

/// `PostUserImage` and `DeleteUserImage` bind `userId` alone, so an undeclared
/// member is never parsed. Their legacy forms take `userId` from the route and
/// bind no query member, except `DeleteUserImageLegacy`'s `[FromQuery] int?
/// index`: the indexed delete takes `index` from its route, and the uploads
/// declare none. Each row was measured on Jellyfin 12.4 (gl-1031).
#[rstest::rstest]
#[case::post_size("POST", "/UserImage?maxWidth=x", StatusCode::NO_CONTENT)]
#[case::delete_size("DELETE", "/UserImage?maxWidth=x", StatusCode::NO_CONTENT)]
#[case::post_image_index("POST", "/UserImage?imageIndex=x", StatusCode::NO_CONTENT)]
#[case::delete_image_index("DELETE", "/UserImage?imageIndex=x", StatusCode::NO_CONTENT)]
#[case::post_user_id("POST", "/UserImage?userId=junk", StatusCode::BAD_REQUEST)]
#[case::delete_user_id("DELETE", "/UserImage?userId=junk", StatusCode::BAD_REQUEST)]
#[case::post_legacy_size(
    "POST",
    "/Users/{user}/Images/Primary?maxWidth=x",
    StatusCode::NO_CONTENT
)]
#[case::delete_legacy_size(
    "DELETE",
    "/Users/{user}/Images/Primary?maxWidth=x",
    StatusCode::NO_CONTENT
)]
#[case::post_legacy_user_id(
    "POST",
    "/Users/{user}/Images/Primary?userId=junk",
    StatusCode::NO_CONTENT
)]
#[case::delete_legacy_user_id(
    "DELETE",
    "/Users/{user}/Images/Primary?userId=junk",
    StatusCode::NO_CONTENT
)]
#[case::post_legacy_index("POST", "/Users/{user}/Images/Primary?index=x", StatusCode::NO_CONTENT)]
#[case::delete_legacy_index(
    "DELETE",
    "/Users/{user}/Images/Primary?index=x",
    StatusCode::BAD_REQUEST
)]
#[case::post_indexed_size(
    "POST",
    "/Users/{user}/Images/Primary/0?maxWidth=x",
    StatusCode::NO_CONTENT
)]
#[case::delete_indexed_size(
    "DELETE",
    "/Users/{user}/Images/Primary/0?maxWidth=x",
    StatusCode::NO_CONTENT
)]
#[case::post_indexed_user_id(
    "POST",
    "/Users/{user}/Images/Primary/0?userId=junk",
    StatusCode::NO_CONTENT
)]
#[case::delete_indexed_user_id(
    "DELETE",
    "/Users/{user}/Images/Primary/0?userId=junk",
    StatusCode::NO_CONTENT
)]
#[case::post_indexed_index(
    "POST",
    "/Users/{user}/Images/Primary/0?index=x",
    StatusCode::NO_CONTENT
)]
#[case::delete_indexed_index(
    "DELETE",
    "/Users/{user}/Images/Primary/0?index=x",
    StatusCode::NO_CONTENT
)]
#[tokio::test]
async fn user_image_writes_bind_what_their_actions_declare(
    #[case] method: &str,
    #[case] route: &str,
    #[case] expected: StatusCode,
) {
    let s = stubs(String::new(), String::new());
    let uri = route.replace("{user}", &USER_ID.to_string());
    let body = (method == "POST").then_some(("image/png", "aGk="));
    let (status, _) = send(&s, method, &uri, body).await;
    assert_eq!(status, expected, "{method} {uri}");
}

/// No item image action declares `userId`, so a malformed one is never seen.
/// Measured on Jellyfin 12.4 (gl-1031).
#[rstest::rstest]
#[case::get("GET", "/Items/{item}/Images/Primary", StatusCode::OK)]
#[case::get_indexed("GET", "/Items/{item}/Images/Primary/0", StatusCode::OK)]
#[case::delete("DELETE", "/Items/{item}/Images/Primary", StatusCode::NO_CONTENT)]
#[tokio::test]
async fn item_image_routes_ignore_a_user_id(
    #[case] method: &str,
    #[case] route: &str,
    #[case] expected: StatusCode,
) {
    let img = TempImage::new(b"POSTER");
    let s = elevated_stubs(img.path(), String::new());
    let uri = format!(
        "{}?userId=junk",
        route.replace("{item}", &ITEM_ID.to_string())
    );
    let (status, _) = send(&s, method, &uri, None).await;
    assert_eq!(status, expected, "{method} {uri}");
}

/// `GetItemImage2` takes `width`, `height`, `quality`, `fillWidth`,
/// `fillHeight` and `blur` from the query string, and the format, the maximum
/// size and the tag from the route, where a query parameter of the same name
/// has no effect.
#[tokio::test]
async fn the_positional_route_reads_its_query_string_transforms() {
    let img = TempImage::new(b"IMAGEBYTES");
    let s = stubs(img.path(), String::new());
    let processor = Arc::new(RecordingProcessor::default());
    let request = Request::builder()
        .uri(format!(
            "/Items/{ITEM_ID}/Images/Primary/0/tag/jpg/100/0/0/0\
             ?width=40&height=30&quality=60&fillWidth=50&fillHeight=20&blur=2\
             &maxWidth=999&maxHeight=999&format=png"
        ))
        .header("Authorization", "Bearer token")
        .body(Body::empty())
        .expect("request");
    let response = create_router(state(&s).with_image_processor(processor.clone()))
        .oneshot(request)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let asked = processor.asked.lock().expect("asked lock");
    let options = asked.first().expect("the image was processed");
    assert_eq!(
        (options.width, options.height, options.quality),
        (Some(40), Some(30), 60)
    );
    assert_eq!(
        (options.fill_width, options.fill_height, options.blur),
        (Some(50), Some(20), Some(2))
    );
    // The route's segments win: 100 wide, a height of `0` as given, JPEG.
    assert_eq!(
        (options.max_width, options.max_height),
        (Some(100), Some(0))
    );
    assert_eq!(
        options.supported_output_formats,
        [ferrofin_model::drawing::ImageFormat::Jpg]
    );
}

/// gl-1212: `GetItemImage2` passes its route's sizes and tag on as given. A
/// `0` size reaches the processor, where `HasDefaultOptions` counts it, so
/// the image is re-encoded; a tag of `0` is a tag, with the year-long
/// caching, its `ETag` and its `304`. Measured on 12.4.
#[tokio::test]
async fn the_positional_route_passes_a_zero_size_and_tag_on() {
    let img = TempImage::new(b"IMAGEBYTES");
    let processor = Arc::new(RecordingProcessor::default());
    let mut s = stubs(img.path(), String::new());
    s.processor = Some(processor.clone());
    let uri = format!("/Items/{ITEM_ID}/Images/Primary/0/0/jpg/0/0/0/0");

    let (status, headers, _) = send_with_headers(&s, &uri, &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(headers["etag"], "\"0\"");
    let options = processor
        .asked
        .lock()
        .expect("asked lock")
        .first()
        .cloned()
        .expect("the image was processed");
    assert_eq!((options.max_width, options.max_height), (Some(0), Some(0)));
    let (status, _, body) = send_with_headers(&s, &uri, &[("If-None-Match", "\"0\"")]).await;
    assert_eq!((status, body.len()), (StatusCode::NOT_MODIFIED, 0));

    // A negative size is passed on too (`HasDefaultOptions` counts it as well).
    let uri = format!("/Items/{ITEM_ID}/Images/Primary/0/tag/jpg/-5/-5/0/0");
    let (status, _, _) = send_with_headers(&s, &uri, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let options = processor
        .asked
        .lock()
        .expect("asked lock")
        .last()
        .cloned()
        .expect("the image was processed");
    assert_eq!(
        (options.max_width, options.max_height),
        (Some(-5), Some(-5))
    );
}

/// gl-1212: `GetItemImage2` takes `imageIndex`, `tag`, `format`, `maxWidth`,
/// `maxHeight`, `percentPlayed` and `unplayedCount` from the route alone, so a
/// malformed one in the query is never read, while the query members it
/// declares are bound. Its route numbers bind as the query binder binds a
/// number: hex, `#`, a sign and surrounding spaces. Measured on 12.4.
#[rstest::rstest]
#[case::query_max_width("0/tag/jpg/100/100/0/0?maxWidth=x", StatusCode::OK)]
#[case::query_max_height("0/tag/jpg/100/100/0/0?maxHeight=x", StatusCode::OK)]
#[case::query_percent_played("0/tag/jpg/100/100/0/0?percentPlayed=x", StatusCode::OK)]
#[case::query_unplayed_count("0/tag/jpg/100/100/0/0?unplayedCount=x", StatusCode::OK)]
#[case::query_image_index("0/tag/jpg/100/100/0/0?imageIndex=x", StatusCode::OK)]
#[case::query_format("0/tag/jpg/100/100/0/0?format=x", StatusCode::OK)]
#[case::query_width("0/tag/jpg/100/100/0/0?width=x", StatusCode::BAD_REQUEST)]
#[case::query_quality("0/tag/jpg/100/100/0/0?quality=x", StatusCode::BAD_REQUEST)]
#[case::query_fill_height("0/tag/jpg/100/100/0/0?fillHeight=x", StatusCode::BAD_REQUEST)]
#[case::query_blur("0/tag/jpg/100/100/0/0?blur=x", StatusCode::BAD_REQUEST)]
#[case::hex_image_index("0x0/tag/jpg/100/100/0/0", StatusCode::OK)]
#[case::hash_max_width("0/tag/jpg/%2364/100/0/0", StatusCode::OK)]
#[case::padded_unplayed_count("0/tag/jpg/100/100/0/%20%200", StatusCode::OK)]
#[case::signed_max_height("0/tag/jpg/100/%2B100/0/0", StatusCode::OK)]
#[case::malformed_image_index("x/tag/jpg/100/100/0/0", StatusCode::BAD_REQUEST)]
#[case::malformed_max_width("0/tag/jpg/x/100/0/0", StatusCode::BAD_REQUEST)]
#[case::malformed_percent_played("0/tag/jpg/100/100/x/0", StatusCode::BAD_REQUEST)]
#[case::hex_percent_played("0/tag/jpg/100/100/0x0/0", StatusCode::BAD_REQUEST)]
#[case::blank_tag("0/%20/jpg/100/100/0/0", StatusCode::BAD_REQUEST)]
#[case::padded_tag("0/%20tag%20/jpg/100/100/0/0", StatusCode::OK)]
#[tokio::test]
async fn the_positional_route_binds_what_its_action_declares(
    #[case] tail: &str,
    #[case] expected: StatusCode,
) {
    let img = TempImage::new(b"POSTER");
    let s = stubs(img.path(), String::new());
    for method in ["GET", "HEAD"] {
        let uri = format!("/Items/{ITEM_ID}/Images/Primary/{tail}");
        let (status, _) = send(&s, method, &uri, None).await;
        assert_eq!(status, expected, "{method} {uri}");
    }
}
