//! Item update / edit domain integration tests: `POST /Items/{id}` edit,
//! `POST /Items/{id}/Refresh`, `POST /Items/{id}/ContentType`, the item metadata
//! editor (`GET /Items/{id}/MetadataEditor`), and external-id descriptors
//! (`GET /Items/{id}/ExternalIdInfos`).
//!
//! Consolidated from `handler_success_paths.rs`, `batch16_handlers.rs`, and
//! `batch14_handlers.rs`. A single harness backs every test: `state()` wires a
//! library resolving one fixed item, a provider that records queued refreshes and
//! advertises one external-id descriptor, and a real localization stub the
//! metadata editor reads.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeConfig, FakeMediaSources, FakeMusic, FakeSearch, FakeSessions, FakeSimilarItems,
    FakeSystem, FakeUserData, FakeUserViews,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::MetadataEditorInfo;
use ferrofin_model::providers::ExternalIdInfo;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::providers::{
    ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use tower::ServiceExt;
use uuid::Uuid;

// A fixed authenticated user id shared across the stubs and the assertions.
const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
// The single item the library resolves, referenced by the editor / external-id
// tests. HSP tests pass their own literal item ids to `state()`.
const ITEM_ID: Uuid = Uuid::from_u128(0x00A1_7E11);
const MISSING_ID: Uuid = Uuid::from_u128(0xDEAD);

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

/// A [`LibraryManager`] resolving a single known item id (any other is `None`);
/// `update_items` succeeds so the edit handler runs end-to-end. The folder
/// fields shape the resolved entity for the folder-refresh (scoped scan) tests,
/// which assert against `scoped_scans`.
struct OkLibrary {
    item_id: Uuid,
    is_folder: bool,
    top_parent_id: Option<Uuid>,
    scoped_scans: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl LibraryManager for OkLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok((id == self.item_id).then(|| {
            let mut entity = base_item_entity(self.item_id);
            entity.is_folder = self.is_folder;
            entity.top_parent_id = self.top_parent_id.map(|id| id.to_string());
            entity
        }))
    }
    async fn queue_library_scan_scoped(&self, library_id: Uuid) -> Result<(), ServiceError> {
        self.scoped_scans.lock().unwrap().push(library_id);
        Ok(())
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        Ok(())
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
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
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

/// A [`DtoService`] projecting each entity into id + name.
struct OkDto;

fn entity_to_dto(item: &BaseItemEntity) -> ferrofin_model::dto::BaseItemDto {
    ferrofin_model::dto::BaseItemDto {
        id: Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil()),
        name: item.name.clone(),
        ..ferrofin_model::dto::BaseItemDto::default()
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
    ) -> Result<ferrofin_model::dto::BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        Ok(items.iter().map(entity_to_dto).collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<ferrofin_model::dto::BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
}

/// A [`ProviderManager`] that records the last queued refresh (so the refresh
/// handler is observable) and advertises a single external-id descriptor (so the
/// external-id / metadata-editor routes return data).
struct RecordingProviders {
    queued: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl ProviderManager for RecordingProviders {
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        _options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        self.queued.lock().unwrap().push(item_id);
        Ok(())
    }
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
        _image_type: ferrofin_model::entities::ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: ferrofin_model::entities::ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &ferrofin_model::providers::RemoteImageQuery,
    ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
        unimplemented!()
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_external_urls(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalUrl>, ServiceError> {
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

/// A [`LocalizationManager`] returning canned cultures/countries/ratings so the
/// metadata-editor handler can build its descriptor. The two cultures share a
/// display name (different casing) to exercise the handler's dedupe.
struct StubLocalization;

impl ferrofin_traits::localization::LocalizationManager for StubLocalization {
    fn get_cultures(&self) -> Vec<ferrofin_model::globalization::CultureDto> {
        vec![
            ferrofin_model::globalization::CultureDto {
                name: "en".to_owned(),
                display_name: "English".to_owned(),
                two_letter_iso_language_name: "en".to_owned(),
                three_letter_iso_language_name: Some("eng".to_owned()),
                three_letter_iso_language_names: vec!["eng".to_owned()],
            },
            ferrofin_model::globalization::CultureDto {
                name: "en-US".to_owned(),
                display_name: "english".to_owned(),
                two_letter_iso_language_name: "en".to_owned(),
                three_letter_iso_language_name: Some("eng".to_owned()),
                three_letter_iso_language_names: vec!["eng".to_owned()],
            },
        ]
    }
    fn get_countries(&self) -> Vec<ferrofin_model::globalization::CountryInfo> {
        vec![ferrofin_model::globalization::CountryInfo::default()]
    }
    fn get_parental_ratings(&self) -> Vec<ferrofin_model::entities_media::ParentalRating> {
        vec![ferrofin_model::entities_media::ParentalRating::new(
            "PG".to_owned(),
            None,
        )]
    }
    fn get_localization_options(&self) -> Vec<ferrofin_model::globalization::LocalizationOption> {
        Vec::new()
    }
    fn get_rating_score(
        &self,
        _rating: &str,
        _country_code: Option<&str>,
    ) -> Option<ferrofin_model::entities_media::ParentalRatingScore> {
        None
    }
}

/// Assembles an [`AppState`] wired for the item-update paths. `queued` records
/// refresh requests; pass a throwaway when they are not asserted.
fn state(item_id: Uuid, queued: Arc<Mutex<Vec<Uuid>>>) -> AppState {
    state_with_library(
        Arc::new(OkLibrary {
            item_id,
            is_folder: false,
            top_parent_id: None,
            scoped_scans: Arc::default(),
        }),
        queued,
    )
}

/// [`state`] with a caller-shaped [`OkLibrary`] (the folder-refresh tests).
fn state_with_library(library: Arc<OkLibrary>, queued: Arc<Mutex<Vec<Uuid>>>) -> AppState {
    AppState::new(
        library,
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(ferrofin_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(RecordingProviders { queued }),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
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
        Arc::new(StubLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
}

/// Drives one request through the router and returns (status, body bytes).
async fn send(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    let queued = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state(ITEM_ID, queued));
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

// ---- from handler_success_paths.rs --------------------------------------------

/// `POST /Items/{itemId}` applies an edited item and returns `204`.
#[tokio::test]
async fn update_item_returns_204() {
    let item_id = Uuid::from_u128(0x59);
    let router = create_router(state(item_id, Arc::new(Mutex::new(Vec::new()))));
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
    let router = create_router(state(
        Uuid::from_u128(0x5A),
        Arc::new(Mutex::new(Vec::new())),
    ));
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
    let queued = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state(item_id, queued.clone()));
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

/// `POST /Items/{itemId}/Refresh` on a library's CollectionFolder queues a scan
/// scoped to that library — never a full all-libraries scan (the stub's
/// unscoped `queue_library_scan` is `unimplemented!`, so a regression to the
/// old behavior fails loudly here).
#[tokio::test]
async fn refresh_library_folder_queues_scoped_scan() {
    let folder_id = Uuid::from_u128(0x11B);
    let scans = Arc::new(Mutex::new(Vec::new()));
    let queued = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state_with_library(
        Arc::new(OkLibrary {
            item_id: folder_id,
            is_folder: true,
            top_parent_id: None, // a CollectionFolder is its own library root
            scoped_scans: scans.clone(),
        }),
        queued.clone(),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{folder_id}/Refresh"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(scans.lock().unwrap().as_slice(), &[folder_id]);
    assert!(
        queued.lock().unwrap().is_empty(),
        "a folder refresh drives the scan, not the provider queue"
    );
}

/// Refreshing a folder nested inside a library (a series/season) scopes the
/// scan to the owning library via `TopParentId`, not the folder's own id.
#[tokio::test]
async fn refresh_nested_folder_scopes_to_owning_library() {
    let series_id = Uuid::from_u128(0x5E1);
    let library_id = Uuid::from_u128(0x11B2);
    let scans = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state_with_library(
        Arc::new(OkLibrary {
            item_id: series_id,
            is_folder: true,
            top_parent_id: Some(library_id),
            scoped_scans: scans.clone(),
        }),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{series_id}/Refresh"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(scans.lock().unwrap().as_slice(), &[library_id]);
}

/// `POST /Items/{itemId}/Refresh` for a missing item is a `404` (never queues).
#[tokio::test]
async fn refresh_missing_item_is_404() {
    let queued = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state(Uuid::from_u128(0x5C), queued.clone()));
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
/// success path needs a full `ServerConfiguration`, exercised in `ferrofin-core`).
#[tokio::test]
async fn content_type_missing_item_is_404() {
    let router = create_router(state(
        Uuid::from_u128(0x5D),
        Arc::new(Mutex::new(Vec::new())),
    ));
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

// ---- from batch16_handlers.rs -------------------------------------------------

#[tokio::test]
async fn metadata_editor_returns_descriptor() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/MetadataEditor"),
        Body::empty(),
    )
    .await;
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
    let (status, _) = send(
        "GET",
        &format!("/Items/{MISSING_ID}/MetadataEditor"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- from batch14_handlers.rs -------------------------------------------------

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
