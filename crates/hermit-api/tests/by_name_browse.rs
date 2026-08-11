//! Success-path tests for the Batch 1 by-name browse controllers
//! (Genres, `MusicGenres`, Studios, Persons, Years, Artists).
//!
//! Each list route projects the library manager's by-name aggregates into a
//! [`QueryResult<BaseItemDto>`]; each `{name}` route resolves one by-name item
//! (or `404`s). These tests drive the real router with stub `hermit-traits`
//! impls that authenticate and return data, asserting the wire shape and status.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeAppHost, FakeConfig, FakeMediaSources, FakeMusic, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSystem, FakeUserData, FakeUserViews,
};
use hermit_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use hermit_db::entities::users::UserEntity;
use hermit_model::data::{BaseItemKind, CollectionType};
use hermit_model::dto::{BaseItemDto, ItemCounts};
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, UserManager};
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use hermit_traits::persistence::ItemWithCounts;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x5150);

/// Minimal authenticated user; every non-id field is a neutral zero value.
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
        username: "bob".to_owned(),
    }
}

/// A named by-name item row (a genre/studio/artist/…); other fields are empty.
fn named_entity(id: Uuid, name: &str) -> BaseItemEntity {
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
        type_: String::new(),
        unrated_type: None,
        width: None,
    }
}

/// A [`LibraryManager`] wired for by-name browse: the aggregate/list/people/
/// filter reads return one seeded row so every Batch 1 route resolves.
struct ByNameLibrary;

const GENRE_ID: Uuid = Uuid::from_u128(0xAA01);
const STUDIO_ID: Uuid = Uuid::from_u128(0xAA02);
const ARTIST_ID: Uuid = Uuid::from_u128(0xAA03);
const PERSON_ID: Uuid = Uuid::from_u128(0xAA04);
const MUSIC_GENRE_ID: Uuid = Uuid::from_u128(0xAA05);
const YEAR_ID: Uuid = Uuid::from_u128(0xAA06);

fn one_aggregate(id: Uuid, name: &str) -> QueryResult<ItemWithCounts> {
    QueryResult::new(
        Some(0),
        Some(1),
        vec![ItemWithCounts {
            item: named_entity(id, name),
            counts: ItemCounts {
                item_count: 7,
                ..ItemCounts::default()
            },
        }],
    )
}

#[async_trait]
impl LibraryManager for ByNameLibrary {
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(one_aggregate(GENRE_ID, "Drama"))
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(one_aggregate(MUSIC_GENRE_ID, "Jazz"))
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(one_aggregate(STUDIO_ID, "A24"))
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(one_aggregate(ARTIST_ID, "Miles Davis"))
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        Ok(one_aggregate(ARTIST_ID, "Miles Davis"))
    }
    /// Backs the `get_named_item` default: resolves the by-name row by kind.
    async fn get_item_list(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let kind = q.include_item_types.first().copied();
        let name = q.name.clone().unwrap_or_default();
        let row = match kind {
            Some(BaseItemKind::Genre) if name == "Drama" => Some(named_entity(GENRE_ID, "Drama")),
            Some(BaseItemKind::MusicGenre) if name == "Jazz" => {
                Some(named_entity(MUSIC_GENRE_ID, "Jazz"))
            }
            Some(BaseItemKind::Studio) if name == "A24" => Some(named_entity(STUDIO_ID, "A24")),
            Some(BaseItemKind::MusicArtist) if name == "Miles Davis" => {
                Some(named_entity(ARTIST_ID, "Miles Davis"))
            }
            Some(BaseItemKind::Person) if name == "Uma" => Some(named_entity(PERSON_ID, "Uma")),
            Some(BaseItemKind::Year) if name == "1999" => Some(named_entity(YEAR_ID, "1999")),
            _ => None,
        };
        Ok(row.into_iter().collect())
    }
    /// Backs the `get_people_items` default for `GET /Persons`.
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        Ok(vec![PeopleEntity {
            id: PERSON_ID.to_string(),
            name: "Uma".to_owned(),
            person_type: Some("Actor".to_owned()),
            ..Default::default()
        }])
    }
    /// Backs the `get_years` default: one distinct production year.
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        Ok(QueryFiltersLegacy {
            years: vec![1999],
            ..QueryFiltersLegacy::default()
        })
    }

    // ---- Remaining methods are never reached by these tests. ----
    async fn get_item_by_id(&self, _id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!()
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
    async fn get_people_names(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(&self, _q: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(&self, _q: &InternalItemsQuery) -> Result<ItemCounts, ServiceError> {
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
        _u: Uuid,
        _c: &hermit_model::configuration::UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_policy(
        &self,
        _u: Uuid,
        _p: &hermit_model::users::UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn clear_profile_image(&self, _u: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`DtoService`] projecting each entity into a `BaseItemDto` (id + name).
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

/// An [`AuthService`]/[`AuthorizationContext`] pair that authenticates as
/// [`USER_ID`] so `RequireAuth` passes.
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
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
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID)),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

fn by_name_state() -> AppState {
    AppState::new(
        Arc::new(ByNameLibrary),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(hermit_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
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

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// GETs `uri` on the by-name router and returns the response.
async fn get(uri: &str) -> axum::response::Response {
    create_router(by_name_state())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn genres_list_returns_query_result() {
    let response = get("/Genres").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["TotalRecordCount"], 1);
    assert_eq!(body["Items"][0]["Name"], "Drama");
}

#[tokio::test]
async fn genres_list_folds_counts_when_include_item_types_set() {
    // `includeItemTypes` non-empty → aggregated counts fold onto ChildCount.
    let response = get("/Genres?includeItemTypes=Movie").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Items"][0]["ChildCount"], 7);
}

#[tokio::test]
async fn genre_by_name_returns_item() {
    let response = get("/Genres/Drama").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Name"], "Drama");
}

#[tokio::test]
async fn genre_by_name_missing_is_404() {
    let response = get("/Genres/Nonexistent").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn music_genres_list_and_by_name() {
    assert_eq!(get("/MusicGenres").await.status(), StatusCode::OK);
    let response = get("/MusicGenres/Jazz").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["Name"], "Jazz");
    assert_eq!(
        get("/MusicGenres/Nope").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn studios_list_and_by_name() {
    let list = get("/Studios").await;
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(json_body(list).await["Items"][0]["Name"], "A24");
    assert_eq!(get("/Studios/A24").await.status(), StatusCode::OK);
    assert_eq!(get("/Studios/Nope").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn artists_list_album_artists_and_by_name() {
    assert_eq!(get("/Artists").await.status(), StatusCode::OK);
    assert_eq!(get("/Artists/AlbumArtists").await.status(), StatusCode::OK);
    let response = get("/Artists/Miles%20Davis").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["Name"], "Miles Davis");
}

#[tokio::test]
async fn persons_list_resolves_people_items() {
    let response = get("/Persons").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Items"][0]["Name"], "Uma");
    // Single person by name.
    let one = get("/Persons/Uma").await;
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(json_body(one).await["Name"], "Uma");
    assert_eq!(get("/Persons/Nobody").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn years_list_and_by_value() {
    let list = get("/Years").await;
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(json_body(list).await["Items"][0]["Name"], "1999");
    let one = get("/Years/1999").await;
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(json_body(one).await["Name"], "1999");
    // Non-positive and absent years are 404.
    assert_eq!(get("/Years/0").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get("/Years/2020").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn by_name_routes_require_auth() {
    // With the default (rejecting) auth fake, the routes are 401 — proving they
    // exist and are guarded, not stubbed 501s.
    let router = create_router(hermit_api::test_support::fake_state());
    for uri in ["/Genres", "/Studios", "/Artists", "/Persons", "/Years"] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} should require auth"
        );
    }
}
