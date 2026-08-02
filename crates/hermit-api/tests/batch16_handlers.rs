//! Batch-16 handler **success-path** tests: the last portable stubs — similar
//! items, item-image write/delete, user-image upload, branding splashscreen,
//! `UserViews/GroupingOptions`, `Library/MediaFolders`, and the item metadata
//! editor.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! compact `hermit-traits` stubs that authenticate as a fixed user and return (or
//! record) canned data; every manager a handler does not touch reuses the
//! `test_support` panic fakes, so a handler that strays into an unexpected seam
//! trips a panic. The genuinely deferred routes (the `MergeVersions`/`SkipIntro`/
//! `SegmentEditor` plugin routes, `Library/VirtualFolders*`, `PhysicalPaths`,
//! `Libraries/AvailableOptions`) stay registered `501` stubs and are covered by
//! the contract-superset test, not here.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
    FakeDisplayPreferences, FakeFileSystem, FakeLyrics, FakeMediaSegments, FakeMediaSources,
    FakeMusic, FakePlaylists, FakeQuickConnect, FakeSearch, FakeSessions, FakeSubtitles,
    FakeSystem, FakeTasks, FakeTrickplay, FakeTvSeries, FakeUserData, minimal_base_item,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::base_items::PeopleEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::branding::BrandingOptions;
use hermit_model::configuration::ServerConfiguration;
use hermit_model::data::CollectionType;
use hermit_model::dto::{BaseItemDto, MetadataEditorInfo, SpecialViewOptionDto};
use hermit_model::dto::{NameIdPair, UserDto};
use hermit_model::entities::ImageType;
use hermit_model::entities::MediaStreamType;
use hermit_model::providers::{
    ExternalIdInfo, ExternalUrl, ImageProviderInfo, RemoteImageInfo, RemoteImageQuery,
};
use hermit_model::querying::QueryFiltersLegacy;
use hermit_model::querying::QueryResult;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{
    LibraryManager, SimilarItemsManager, SimilarItemsRecommendation, UserManager, UserViewManager,
};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::persistence::ItemWithCounts;
use hermit_traits::providers::{
    ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use hermit_traits::system::{ServerApplicationPaths, SystemManager};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
const ITEM_ID: Uuid = Uuid::from_u128(0x00A1_7E11);
const MISSING_ID: Uuid = Uuid::from_u128(0xDEAD);

// ---- shared auth --------------------------------------------------------------

/// An auth pair that authenticates every request as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "tester")),
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
            user: Some(user_entity(USER_ID, "tester")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

// ---- entity helpers -----------------------------------------------------------

fn item(id: Uuid, name: &str) -> BaseItemEntity {
    minimal_base_item(id, name, "Movie")
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

// ---- library stub -------------------------------------------------------------

/// A [`LibraryManager`] that resolves [`ITEM_ID`] (and nothing else) and returns a
/// canned set of collection folders for the media-folder route.
#[derive(Default)]
struct StubLibrary {
    /// Records each `(item_id, image_type, index1, index2)` swap the handler asks
    /// for, so the reorder test can assert the request reached the manager.
    swaps: Mutex<Vec<(Uuid, ImageType, i32, i32)>>,
}

#[async_trait]
impl LibraryManager for StubLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == ITEM_ID).then(|| item(ITEM_ID, "The Item")))
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
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn query_items(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
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
        _t: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn create_items(
        &self,
        _i: &[BaseItemEntity],
        _o: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_items(
        &self,
        _i: &[BaseItemEntity],
        _o: Option<Uuid>,
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
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _t: MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

// ---- user-view stub -----------------------------------------------------------

/// A [`UserViewManager`] returning two collection folders as the user's views.
struct StubUserViews;

#[async_trait]
impl UserViewManager for StubUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![
            minimal_base_item(Uuid::from_u128(0x101), "Shows", "CollectionFolder"),
            minimal_base_item(Uuid::from_u128(0x102), "Movies", "CollectionFolder"),
        ])
    }
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(user_id).await
    }
    async fn get_latest_items(
        &self,
        _user_id: Uuid,
        _options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        Ok(Vec::new())
    }
}

// ---- user stub ----------------------------------------------------------------

/// A [`UserManager`] that resolves [`USER_ID`] and records `save_profile_image`.
struct StubUsers {
    saved: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl UserManager for StubUsers {
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok((id == USER_ID).then(|| user_entity(USER_ID, "tester")))
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
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!()
    }
    async fn get_user_dto(
        &self,
        _user: &UserEntity,
        _server_id: Option<String>,
    ) -> Result<UserDto, ServiceError> {
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
}

// ---- similar-items stub -------------------------------------------------------

/// A [`SimilarItemsManager`] returning two similar items.
struct StubSimilar;

#[async_trait]
impl SimilarItemsManager for StubSimilar {
    async fn get_similar_items(
        &self,
        _item_id: Uuid,
        _exclude_artist_ids: &[Uuid],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
        _limit: Option<i32>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![
            item(Uuid::from_u128(0xA1), "Similar A"),
            item(Uuid::from_u128(0xA2), "Similar B"),
        ])
    }
    async fn get_movie_recommendations(
        &self,
        _user_id: Option<Uuid>,
        _parent_id: Uuid,
        _category_limit: i32,
        _item_limit: i32,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        unimplemented!()
    }
}

// ---- provider stub ------------------------------------------------------------

/// A [`ProviderManager`] recording `save_image`/`delete_image` and advertising a
/// descriptor for the metadata editor.
struct StubProviders {
    saved: Arc<Mutex<Vec<(Uuid, String)>>>,
    deleted: Arc<Mutex<Vec<(Uuid, i32)>>>,
}

#[async_trait]
impl ProviderManager for StubProviders {
    async fn queue_refresh(
        &self,
        _item_id: Uuid,
        _o: &MetadataRefreshOptions,
        _p: RefreshPriority,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn refresh_full_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn refresh_single_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        Ok(ItemUpdateType::None)
    }
    async fn save_image_from_url(
        &self,
        _i: Uuid,
        _u: &str,
        _t: ImageType,
        _x: Option<i32>,
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
        _i: Uuid,
        _q: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_remote_image_provider_info(
        &self,
        _i: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn save_metadata(&self, _i: Uuid, _u: ItemUpdateType) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_external_urls(&self, _i: Uuid) -> Result<Vec<ExternalUrl>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_external_id_infos(&self, _i: Uuid) -> Result<Vec<ExternalIdInfo>, ServiceError> {
        Ok(vec![ExternalIdInfo::new(
            "Tmdb".into(),
            "tmdb".into(),
            None,
        )])
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<hermit_model::configuration::MetadataPluginSummary>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_metadata_options(
        &self,
        _i: Uuid,
    ) -> Result<hermit_model::configuration::MetadataOptions, ServiceError> {
        Ok(hermit_model::configuration::MetadataOptions::default())
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(Vec::new())
    }
}

// ---- dto stub -----------------------------------------------------------------

/// A [`DtoService`] projecting each entity to a name-carrying [`BaseItemDto`].
struct StubDto;

#[async_trait]
impl DtoService for StubDto {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        Ok(None)
    }
    async fn get_base_item_dto(
        &self,
        item: &BaseItemEntity,
        _o: &DtoOptions,
        _u: Option<&UserEntity>,
        _owner: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(BaseItemDto {
            name: item.name.clone(),
            ..BaseItemDto::default()
        })
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _o: &DtoOptions,
        _u: Option<&UserEntity>,
        _owner: Option<Uuid>,
        _skip: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|i| BaseItemDto {
                name: i.name.clone(),
                ..BaseItemDto::default()
            })
            .collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _o: &DtoOptions,
        _tagged: Option<&[Uuid]>,
        _u: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(BaseItemDto {
            name: item.name.clone(),
            ..BaseItemDto::default()
        })
    }
}

// ---- localization stub --------------------------------------------------------

/// A [`LocalizationManager`] returning small canned reference sets, including a
/// duplicate-display-name culture to exercise the metadata-editor dedup pass.
struct StubLocalization;

impl hermit_traits::localization::LocalizationManager for StubLocalization {
    fn get_cultures(&self) -> Vec<hermit_model::globalization::CultureDto> {
        vec![
            hermit_model::globalization::CultureDto {
                name: "en".to_owned(),
                display_name: "English".to_owned(),
                two_letter_iso_language_name: "en".to_owned(),
                three_letter_iso_language_name: Some("eng".to_owned()),
                three_letter_iso_language_names: vec!["eng".to_owned()],
            },
            // Duplicate display name (different casing) — must be deduped.
            hermit_model::globalization::CultureDto {
                name: "en-US".to_owned(),
                display_name: "english".to_owned(),
                two_letter_iso_language_name: "en".to_owned(),
                three_letter_iso_language_name: Some("eng".to_owned()),
                three_letter_iso_language_names: vec!["eng".to_owned()],
            },
        ]
    }
    fn get_countries(&self) -> Vec<hermit_model::globalization::CountryInfo> {
        vec![hermit_model::globalization::CountryInfo::default()]
    }
    fn get_parental_ratings(&self) -> Vec<hermit_model::entities_media::ParentalRating> {
        vec![hermit_model::entities_media::ParentalRating::new(
            "PG".to_owned(),
            None,
        )]
    }
    fn get_localization_options(&self) -> Vec<hermit_model::globalization::LocalizationOption> {
        Vec::new()
    }
    fn get_rating_score(
        &self,
        _rating: &str,
        _country_code: Option<&str>,
    ) -> Option<hermit_model::entities_media::ParentalRatingScore> {
        None
    }
}

// ---- config stub (branding + paths) -------------------------------------------

/// A [`ServerApplicationPaths`] whose `data_path` is a per-test temp directory.
struct TmpPaths {
    data: String,
}

impl ServerApplicationPaths for TmpPaths {
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
        self.data.clone()
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

/// A [`ServerConfigurationManager`] backed by an in-memory branding record and a
/// temp data path, recording `update_branding` so the write routes can be checked.
struct StubConfig {
    data_path: String,
    branding: Mutex<BrandingOptions>,
}

#[async_trait]
impl ServerConfigurationManager for StubConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(TmpPaths {
            data: self.data_path.clone(),
        })
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_configuration(&self, _c: &ServerConfiguration) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Ok(self.branding.lock().expect("lock").clone())
    }
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        *self.branding.lock().expect("lock") = branding.clone();
        Ok(())
    }
}

// ---- state builder ------------------------------------------------------------

struct Stubs {
    library: Arc<StubLibrary>,
    user_views: Arc<StubUserViews>,
    users: Arc<StubUsers>,
    similar: Arc<StubSimilar>,
    providers: Arc<StubProviders>,
    config: Arc<StubConfig>,
    system: Arc<dyn SystemManager>,
}

fn stubs(branding: BrandingOptions, data_path: &str) -> Stubs {
    Stubs {
        library: Arc::new(StubLibrary::default()),
        user_views: Arc::new(StubUserViews),
        users: Arc::new(StubUsers {
            saved: Arc::new(Mutex::new(Vec::new())),
        }),
        similar: Arc::new(StubSimilar),
        providers: Arc::new(StubProviders {
            saved: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
        }),
        config: Arc::new(StubConfig {
            data_path: data_path.to_owned(),
            branding: Mutex::new(branding),
        }),
        system: Arc::new(FakeSystem),
    }
}

fn state(s: &Stubs) -> AppState {
    AppState::new(
        s.library.clone(),
        s.users.clone(),
        s.user_views.clone(),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        s.system.clone(),
        Arc::new(FakeAppHost),
        s.config.clone(),
        s.providers.clone(),
        Arc::new(FakeMusic),
        s.similar.clone(),
        Arc::new(FakeSearch),
        Arc::new(StubDto),
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
        Arc::new(StubLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
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

fn default_branding() -> BrandingOptions {
    BrandingOptions::default()
}

// ---- similar items ------------------------------------------------------------

#[tokio::test]
async fn similar_items_returns_two() {
    let s = stubs(default_branding(), "/tmp");
    for kind in ["Albums", "Artists", "Items", "Movies", "Trailers"] {
        let (status, body) = send(&s, "GET", &format!("/{kind}/{ITEM_ID}/Similar"), None).await;
        assert_eq!(status, StatusCode::OK, "{kind}");
        let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("similar");
        assert_eq!(result.items.len(), 2, "{kind}");
        assert_eq!(result.items[0].name.as_deref(), Some("Similar A"));
    }
}

// ---- item image write/delete --------------------------------------------------

#[tokio::test]
async fn set_item_image_saves_and_returns_204() {
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
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
    let s = stubs(default_branding(), "/tmp");
    let (status, _) = send(&s, "POST", "/UserImage", Some(("text/plain", "aGk="))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---- grouping options ---------------------------------------------------------

#[tokio::test]
async fn grouping_options_returns_name_sorted_views() {
    let s = stubs(default_branding(), "/tmp");
    let (status, body) = send(&s, "GET", "/UserViews/GroupingOptions", None).await;
    assert_eq!(status, StatusCode::OK);
    let opts: Vec<SpecialViewOptionDto> = serde_json::from_slice(&body).expect("options");
    assert_eq!(opts.len(), 2);
    // Name-sorted: Movies before Shows.
    assert_eq!(opts[0].name.as_deref(), Some("Movies"));
    assert_eq!(opts[1].name.as_deref(), Some("Shows"));
    // Ids are dashless guids.
    assert!(opts[0].id.as_deref().is_some_and(|i| !i.contains('-')));
}

// ---- media folders ------------------------------------------------------------

#[tokio::test]
async fn media_folders_returns_collection_folders() {
    let s = stubs(default_branding(), "/tmp");
    let (status, body) = send(&s, "GET", "/Library/MediaFolders", None).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("folders");
    assert_eq!(result.items.len(), 2);
}

// ---- metadata editor ----------------------------------------------------------

#[tokio::test]
async fn metadata_editor_returns_descriptor() {
    let s = stubs(default_branding(), "/tmp");
    let (status, body) = send(&s, "GET", &format!("/Items/{ITEM_ID}/MetadataEditor"), None).await;
    assert_eq!(status, StatusCode::OK);
    let info: MetadataEditorInfo = serde_json::from_slice(&body).expect("editor");
    // A plain library item (e.g. a Movie) gets an empty ContentTypeOptions —
    // Jellyfin only populates it for a collection-folder whose content type is
    // configurable. See get_metadata_editor.
    assert!(info.content_type_options.is_empty());
    assert_eq!(info.external_id_infos.len(), 1);
}

#[tokio::test]
async fn metadata_editor_missing_item_is_404() {
    let s = stubs(default_branding(), "/tmp");
    let (status, _) = send(
        &s,
        "GET",
        &format!("/Items/{MISSING_ID}/MetadataEditor"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- branding splashscreen ----------------------------------------------------

#[tokio::test]
async fn splashscreen_upload_then_get_then_delete() {
    let dir = std::env::temp_dir().join(format!("hermit-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let data_path = dir.to_string_lossy().into_owned();

    let mut branding = default_branding();
    branding.splashscreen_enabled = true;
    let s = stubs(branding, &data_path);

    // Upload: writes <data>/splashscreen-upload.png and records the location.
    let (status, _) = send(
        &s,
        "POST",
        "/Branding/Splashscreen",
        Some(("image/png", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let recorded = s.config.get_branding().await.expect("branding");
    let location = recorded.splashscreen_location.expect("location");
    assert!(std::path::Path::new(&location).is_file());

    // GET: serves the uploaded file.
    let (status, body) = send(&s, "GET", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hi");

    // DELETE: removes the file and clears the location.
    let (status, _) = send(&s, "DELETE", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!std::path::Path::new(&location).exists());
    assert!(
        s.config
            .get_branding()
            .await
            .expect("branding")
            .splashscreen_location
            .is_none()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn splashscreen_get_disabled_is_404() {
    let s = stubs(default_branding(), "/tmp");
    let (status, _) = send(&s, "GET", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn splashscreen_get_enabled_but_no_file_is_404() {
    let mut branding = default_branding();
    branding.splashscreen_enabled = true;
    // A data path with no splashscreen.png present.
    let s = stubs(branding, "/nonexistent-hermit-data-dir");
    let (status, _) = send(&s, "GET", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn splashscreen_upload_bad_content_type_is_400() {
    let s = stubs(default_branding(), "/tmp");
    let (status, _) = send(
        &s,
        "POST",
        "/Branding/Splashscreen",
        Some(("text/plain", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn splashscreen_delete_no_location_is_204() {
    let s = stubs(default_branding(), "/tmp");
    let (status, _) = send(&s, "DELETE", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
