//! Instant-mix handler tests: item / song / music-genre / artist instant mix.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate and return canned data, asserting
//! the success status and the wire-body shape. Managers a given handler never
//! touches reuse the `test_support` panic fakes, catching a handler that strays.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAppHost, FakeConfig, FakeMediaSources, FakeProviders, FakeSessions, FakeSystem,
    FakeUserData, FakeUserViews,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::data::{BaseItemKind, MediaType};
use hermit_model::dto::{BaseItemDto, RecommendationType};
use hermit_model::entities::MediaStreamType;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_model::search::{SearchHint, SearchQuery};
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{
    LibraryManager, MusicManager, SearchManager, SearchResult, SimilarItemsManager,
    SimilarItemsRecommendation, UserManager,
};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::persistence::ItemWithCounts;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x1234_5678);
const SEED_ID: Uuid = Uuid::from_u128(0xBEEF);

/// Builds a minimal [`UserEntity`] with the given id/name; every other field is a
/// neutral zero value ([`UserEntity`] has no `Default`).
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

/// Builds a minimal [`BaseItemEntity`] with the given id + name.
fn item_entity(id: Uuid, name: &str) -> BaseItemEntity {
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

/// A [`DtoService`] projecting each row to a minimal DTO carrying id + name.
struct OkDto;

fn to_dto(item: &BaseItemEntity) -> BaseItemDto {
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
        Ok(to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items.iter().map(to_dto).collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(to_dto(item))
    }
}

/// A [`LibraryManager`] backing the instant-mix seed lookups; unused methods panic.
struct StubLibrary;

#[async_trait]
impl LibraryManager for StubLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        // The instant-mix seed resolves; any other id is absent (→ 404).
        Ok((id == SEED_ID).then(|| item_entity(SEED_ID, "Seed")))
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![item_entity(Uuid::from_u128(0x11), "Result")],
        ))
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![ItemWithCounts {
                item: item_entity(Uuid::from_u128(0x21), "Action"),
                counts: hermit_model::dto::ItemCounts::default(),
            }],
        ))
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![ItemWithCounts {
                item: item_entity(Uuid::from_u128(0x22), "Jazz"),
                counts: hermit_model::dto::ItemCounts::default(),
            }],
        ))
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        Ok(QueryFiltersLegacy {
            genres: vec!["Action".to_owned()],
            tags: vec!["Cult".to_owned()],
            official_ratings: vec!["PG".to_owned()],
            years: vec![1999],
        })
    }
    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        Ok(match stream_type {
            MediaStreamType::Audio => vec!["eng".to_owned(), "deu".to_owned()],
            MediaStreamType::Subtitle => vec!["fra".to_owned()],
            _ => Vec::new(),
        })
    }
    // Remaining methods are never reached by these tests.
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
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`MusicManager`] returning a two-song mix for any seed.
struct StubMusic;

fn mix() -> Vec<BaseItemEntity> {
    vec![
        item_entity(Uuid::from_u128(0x31), "Song A"),
        item_entity(Uuid::from_u128(0x32), "Song B"),
    ]
}

#[async_trait]
impl MusicManager for StubMusic {
    async fn get_instant_mix_from_item(
        &self,
        _item_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(mix())
    }
    async fn get_instant_mix_from_artist(
        &self,
        _artist_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(mix())
    }
    async fn get_instant_mix_from_genres(
        &self,
        _genres: &[String],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(mix())
    }
}

/// A [`SimilarItemsManager`] returning one recommendation category.
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
        unimplemented!()
    }
    async fn get_movie_recommendations(
        &self,
        _user_id: Option<Uuid>,
        _parent_id: Uuid,
        _category_limit: i32,
        _item_limit: i32,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        Ok(vec![SimilarItemsRecommendation {
            baseline_item_name: "Because you watched Alien".to_owned(),
            category_id: Uuid::from_u128(0x41),
            recommendation_type: RecommendationType::SimilarToRecentlyPlayed,
            items: vec![item_entity(Uuid::from_u128(0x42), "Aliens")],
        }])
    }
}

/// A [`SearchManager`] returning one hint.
struct StubSearch;

#[async_trait]
impl SearchManager for StubSearch {
    async fn get_search_hints(
        &self,
        query: &SearchQuery,
    ) -> Result<QueryResult<SearchHint>, ServiceError> {
        let hint = SearchHint {
            item_id: Uuid::from_u128(0x51),
            id: Uuid::from_u128(0x51),
            name: Some("Matrix".to_owned()),
            matched_term: Some(query.search_term.clone()),
            index_number: None,
            production_year: None,
            parent_index_number: None,
            primary_image_tag: None,
            thumb_image_tag: None,
            thumb_image_item_id: None,
            backdrop_image_tag: None,
            backdrop_image_item_id: None,
            type_: BaseItemKind::Movie,
            is_folder: Some(false),
            run_time_ticks: None,
            media_type: MediaType::Video,
            start_date: None,
            end_date: None,
            series: None,
            status: None,
            album: None,
            album_id: None,
            album_artist: None,
            artists: Vec::new(),
            song_count: None,
            episode_count: None,
            channel_id: None,
            channel_name: None,
            primary_image_aspect_ratio: None,
        };
        Ok(QueryResult::new(Some(0), Some(1), vec![hint]))
    }
    async fn get_search_results(
        &self,
        _query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, ServiceError> {
        unimplemented!()
    }
}

/// Builds an [`AppState`] wired with the instant-mix stubs.
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
        Arc::new(FakeProviders),
        Arc::new(StubMusic),
        Arc::new(StubSimilar),
        Arc::new(StubSearch),
        Arc::new(OkDto),
        Arc::new(OkAuth),
        Arc::new(OkAuth),
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

/// Drives one GET request through the router and returns (status, body bytes).
async fn get(uri: &str) -> (StatusCode, Vec<u8>) {
    let router = create_router(state());
    let response = router
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("Authorization", "Bearer token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

#[tokio::test]
async fn item_instant_mix_returns_songs() {
    let (status, body) = get(&format!("/Items/{SEED_ID}/InstantMix?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("mix");
    // Total is the pre-limit count (2); the limit truncates to 1 returned item.
    assert_eq!(result.total_record_count, 2);
    assert_eq!(result.items.len(), 1);
}

#[tokio::test]
async fn songs_instant_mix_missing_seed_is_404() {
    let missing = Uuid::from_u128(0x9999);
    let (status, _) = get(&format!("/Songs/{missing}/InstantMix")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn music_genre_instant_mix_by_name_returns_songs() {
    let (status, body) = get("/MusicGenres/Jazz/InstantMix").await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("mix");
    assert_eq!(result.items.len(), 2);
}

#[tokio::test]
async fn artists_instant_mix_by_id_returns_songs() {
    let (status, body) = get(&format!("/Artists/InstantMix?id={SEED_ID}")).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("mix");
    assert_eq!(result.items.len(), 2);
}
