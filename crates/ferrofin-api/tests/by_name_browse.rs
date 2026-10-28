//! Success-path tests for the Batch 1 by-name browse controllers
//! (Genres, `MusicGenres`, Studios, Persons, Years, Artists).
//!
//! Each list route projects the library manager's by-name aggregates into a
//! [`QueryResult<BaseItemDto>`]; each `{name}` route resolves one by-name item
//! (or `404`s). These tests drive the real router with stub `ferrofin-traits`
//! impls that authenticate and return data, asserting the wire shape and status.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeConfig, FakeMediaSources, FakeMusic, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSystem, FakeUserData, FakeUserViews,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::{BaseItemDto, ItemCounts};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::persistence::ItemWithCounts;
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
        normalized_username: "BOB".to_owned(),
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

/// A [`LibraryManager`] wired for by-name browse: the aggregate/list/people/
/// filter reads return one seeded row so every Batch 1 route resolves.
///
/// `created` records every `CreateItemByName` the handlers trigger, so a test
/// can assert the slug branch resolves WITHOUT materializing anything; `seen`
/// records the query each by-name list ran with.
#[derive(Default, Clone)]
struct ByNameLibrary {
    created: Arc<std::sync::Mutex<Vec<(BaseItemKind, String)>>>,
    seen: Arc<std::sync::Mutex<Vec<InternalItemsQuery>>>,
    /// The people queries `GET /Persons` ran with.
    seen_people: Arc<std::sync::Mutex<Vec<InternalPeopleQuery>>>,
}

impl ByNameLibrary {
    /// Records the query a by-name list ran with.
    fn saw(&self, query: &InternalItemsQuery) {
        self.seen.lock().expect("lock").push(query.clone());
    }
}

const GENRE_ID: Uuid = Uuid::from_u128(0xAA01);
const STUDIO_ID: Uuid = Uuid::from_u128(0xAA02);
const ARTIST_ID: Uuid = Uuid::from_u128(0xAA03);
const PERSON_ID: Uuid = Uuid::from_u128(0xAA04);
const MUSIC_GENRE_ID: Uuid = Uuid::from_u128(0xAA05);
const SLUG_GENRE_ID: Uuid = Uuid::from_u128(0xAA07);
const LITERAL_HYPHEN_GENRE_ID: Uuid = Uuid::from_u128(0xAA08);
const YEAR_ID: Uuid = Uuid::from_u128(0xAA06);
const MUSIC_LIBRARY_ID: Uuid = Uuid::from_u128(0xBB01);
const MUSIC_VIDEOS_LIBRARY_ID: Uuid = Uuid::from_u128(0xBB02);
const MOVIES_LIBRARY_ID: Uuid = Uuid::from_u128(0xBB03);
const MUSIC_FOLDER_ID: Uuid = Uuid::from_u128(0xBB04);

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

    async fn get_genres(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.saw(q);
        Ok(one_aggregate(GENRE_ID, "Drama"))
    }
    async fn get_music_genres(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.saw(q);
        Ok(one_aggregate(MUSIC_GENRE_ID, "Jazz"))
    }
    async fn get_studios(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.saw(q);
        Ok(one_aggregate(STUDIO_ID, "A24"))
    }
    async fn get_artists(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.saw(q);
        Ok(one_aggregate(ARTIST_ID, "Miles Davis"))
    }
    async fn get_album_artists(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.saw(q);
        Ok(one_aggregate(ARTIST_ID, "Miles Davis"))
    }
    /// `CreateItemByName<T>`: resolve, and mint a row when nothing matches —
    /// what the concrete manager does, and what the slug branch must NOT do.
    async fn get_named_item(
        &self,
        kind: BaseItemKind,
        name: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        if let Some(found) = self.find_named_item(kind, name).await? {
            return Ok(Some(found));
        }
        // Only the kinds `by_name_store::PROVISIONED_KINDS` mints, plus Year:
        // `LibraryManager.GetPerson` is a plain lookup on both trees and
        // creates nothing, so a missing person stays missing.
        if !matches!(
            kind,
            BaseItemKind::Genre
                | BaseItemKind::MusicGenre
                | BaseItemKind::Studio
                | BaseItemKind::MusicArtist
        ) {
            return Ok(None);
        }
        self.created
            .lock()
            .expect("lock")
            .push((kind, name.to_owned()));
        Ok(Some(named_entity(Uuid::from_u128(0xAAFF), name)))
    }
    /// Backs the `find_named_item` default: resolves the by-name row by kind,
    /// creating nothing.
    async fn get_item_list(
        &self,
        q: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let kind = q.include_item_types.first().copied();
        let name = q.name.clone().unwrap_or_default();
        let row = match kind {
            Some(BaseItemKind::Genre) if name == "Drama" => Some(named_entity(GENRE_ID, "Drama")),
            // The slug-branch fixture: a genre whose real name carries the
            // character `BaseItem.SlugChar` stands in for on the wire.
            Some(BaseItemKind::Genre) if name == "R&B" => Some(named_entity(SLUG_GENRE_ID, "R&B")),
            // A genre whose real name CONTAINS the slug char. Upstream can
            // never reach it — `GetItemFromSlugName` only ever asks for the
            // three substitutions — and neither may Ferrofin.
            Some(BaseItemKind::Genre) if name == "Sci-Fi" => {
                Some(named_entity(LITERAL_HYPHEN_GENRE_ID, "Sci-Fi"))
            }
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
    /// No content restrictions in this fixture.
    async fn build_access_filter(
        &self,
        _user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<Option<InternalItemsQuery>, ServiceError> {
        Ok(None)
    }
    /// Backs the `get_people_items` default for `GET /Persons`.
    async fn get_people(&self, q: &InternalPeopleQuery) -> Result<Vec<PeopleEntity>, ServiceError> {
        self.seen_people.lock().expect("lock").push(q.clone());
        // Nobody is favorited in this fixture: a favorites-scoped query (the
        // handler must fold `Filters=IsFavorite` onto `is_favorite`) is empty.
        if q.is_favorite == Some(true) {
            return Ok(Vec::new());
        }
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
        q: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        self.saw(q);
        Ok(QueryFiltersLegacy {
            years: vec![1999],
            ..QueryFiltersLegacy::default()
        })
    }

    /// The `/Genres?parentId=` parents: libraries of each collection type, plus
    /// a plain folder whose `Data` claims music (not an `ICollectionFolder`).
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        let (type_, collection_type) = match id {
            MUSIC_LIBRARY_ID => ("CollectionFolder", "music"),
            MUSIC_VIDEOS_LIBRARY_ID => ("CollectionFolder", "musicvideos"),
            MOVIES_LIBRARY_ID => ("CollectionFolder", "movies"),
            MUSIC_FOLDER_ID => ("Folder", "music"),
            _ => return Ok(None),
        };
        let mut row = named_entity(id, "Library");
        row.type_ = format!("MediaBrowser.Controller.Entities.{type_}");
        row.data = Some(format!(r#"{{"CollectionType":"{collection_type}"}}"#));
        Ok(Some(row))
    }

    // ---- Remaining methods are never reached by these tests. ----
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
        _stream_type: ferrofin_model::entities::MediaStreamType,
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

/// An [`AuthService`]/[`AuthorizationContext`] pair for an API key: an
/// authenticated request with no user, as `AuthorizationContext` builds it for
/// a server key.
struct ApiKeyAuth;

impl ApiKeyAuth {
    fn info() -> AuthorizationInfo {
        AuthorizationInfo {
            is_api_key: true,
            is_authenticated: true,
            ..AuthorizationInfo::default()
        }
    }
}

#[async_trait]
impl AuthService for ApiKeyAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(Self::info())
    }
}

#[async_trait]
impl AuthorizationContext for ApiKeyAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(Self::info())
    }
}

fn by_name_state() -> AppState {
    by_name_state_with(Arc::new(OkDto), ByNameLibrary::default())
}

/// The by-name test state over a caller-owned [`ByNameLibrary`], so a test can
/// read back what the handlers created.
fn by_name_state_with_library(library: ByNameLibrary) -> AppState {
    by_name_state_with(Arc::new(OkDto), library)
}

/// The by-name test state, with the DTO service injected so a test can observe
/// the [`DtoOptions`] the handlers build.
fn by_name_state_with_dto(dto: Arc<dyn DtoService>) -> AppState {
    by_name_state_with(dto, ByNameLibrary::default())
}

/// The by-name test state with both seams injected.
fn by_name_state_with(dto: Arc<dyn DtoService>, library: ByNameLibrary) -> AppState {
    by_name_state_authed(dto, library, Arc::new(OkAuth))
}

/// The by-name test state with both seams injected, authenticating every
/// request through `auth`.
fn by_name_state_authed<A: AuthService + AuthorizationContext + 'static>(
    dto: Arc<dyn DtoService>,
    library: ByNameLibrary,
    auth: Arc<A>,
) -> AppState {
    AppState::new(
        Arc::new(library),
        Arc::new(OkUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(ferrofin_api::test_support::FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        dto,
        auth.clone(),
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
    // `includeItemTypes` non-empty → the controller adds `ItemCounts` to the
    // fields and `RequestHelpers.CreateQueryResult` stamps the ten counts,
    // `ChildCount` being `ItemCounts.ItemCount` (v12 `RequestHelpers.cs:166-178`;
    // `BuildItemCountsByCleanName` sets it to `TotalItemCount()`).
    let response = get("/Genres?includeItemTypes=Movie").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Items"][0]["ChildCount"], 7);
    assert_eq!(body["Items"][0]["ProgramCount"], 0);
    assert_eq!(body["Items"][0]["MusicVideoCount"], 0);

    // …and `fields=ItemCounts` asks for the same thing without a type filter.
    let response = get("/Genres?fields=ItemCounts").await;
    let body = json_body(response).await;
    assert_eq!(body["Items"][0]["ChildCount"], 7);

    // Neither → no count fields at all.
    let response = get("/Genres").await;
    let body = json_body(response).await;
    assert!(body["Items"][0].get("ChildCount").is_none());
}

/// `GenresController.GetGenres` answers from `GetMusicGenres` when the parent
/// is a music or music-videos collection folder (`GenresController.cs:141-150`);
/// any other parent, or none, keeps the `Genre` rows. The fake serves "Jazz"
/// from `get_music_genres` and "Drama" from `get_genres`.
#[rstest::rstest]
#[case::music_library(Some(MUSIC_LIBRARY_ID), MUSIC_GENRE_ID, "Jazz")]
#[case::music_videos_library(Some(MUSIC_VIDEOS_LIBRARY_ID), MUSIC_GENRE_ID, "Jazz")]
#[case::movies_library(Some(MOVIES_LIBRARY_ID), GENRE_ID, "Drama")]
#[case::plain_folder_claiming_music(Some(MUSIC_FOLDER_ID), GENRE_ID, "Drama")]
#[case::no_parent(None, GENRE_ID, "Drama")]
#[tokio::test]
async fn genres_list_switches_to_music_genres_under_a_music_library(
    #[case] parent: Option<Uuid>,
    #[case] id: Uuid,
    #[case] name: &str,
) {
    let uri = parent.map_or_else(
        || "/Genres".to_owned(),
        |parent| format!("/Genres?parentId={}", parent.simple()),
    );
    let response = get(&uri).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Items"][0]["Name"], name, "{uri}");
    assert_eq!(body["Items"][0]["Id"], id.simple().to_string(), "{uri}");
}

#[tokio::test]
async fn genre_by_name_returns_item() {
    let response = get("/Genres/Drama").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Name"], "Drama");
}

/// Upstream's `item ??= new Genre()`: `GenresController.GetGenre` has NO
/// not-found arm on either tree, so an unresolvable name is a `200` carrying a
/// default-constructed row — `Guid.Empty` id, no `Name`, no `Path`.
#[tokio::test]
async fn genre_by_name_missing_is_the_empty_item_not_a_404() {
    let response = get("/Genres/Diag-No-Such").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Id"], "00000000000000000000000000000000");
    assert!(body.get("Name").is_none(), "the empty item has no name");
    assert!(body.get("Path").is_none(), "the empty item has no path");
}

/// The slug branch: `'-'` stands in for `&`, `/` and `?` on the wire, so
/// `/Genres/R-B` must resolve the genre actually named `R&B`
/// (`GenresController.GetItemFromSlugName`, identical on v10.11.8 and master).
#[tokio::test]
async fn genre_by_slug_name_resolves_through_the_ampersand_substitution() {
    let response = get("/Genres/R-B").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["Name"], "R&B");
}

/// …and it tries EXACTLY three candidates. A name containing the slug char is
/// read as a slug and nothing else, so a genre literally named `Sci-Fi` is
/// unreachable — upstream's own consequence, verified byte-identical on
/// v10.11.8 and master (`GetItemFromSlugName` substitutes `&`, `/`, `?` and
/// returns). An earlier cut of this port added a fourth lookup on the literal
/// name, which matched neither tree; this test is what stops it coming back.
/// The fake resolves `Sci-Fi` when asked for it by that exact name, so a fourth
/// candidate would return the row and fail the assertion.
#[tokio::test]
async fn a_hyphenated_name_is_only_ever_read_as_a_slug() {
    let response = get("/Genres/Sci-Fi").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body["Id"], "00000000000000000000000000000000",
        "upstream answers the empty Genre() here; reaching the literal row is a divergence"
    );
    assert!(body.get("Name").is_none());
}

/// …and the slug branch must never MATERIALIZE. Upstream's slug lookups are
/// plain `GetItemList` calls; only the non-slug branch is `CreateItemByName`.
/// The fake records every by-name creation, so a hyphenated miss that wrote a
/// row would show up here.
#[tokio::test]
async fn a_slug_miss_creates_nothing() {
    let library = ByNameLibrary::default();
    let created = Arc::clone(&library.created);
    let state = by_name_state_with_library(library);
    let response = create_router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/Genres/Diag-No-Such")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        created.lock().unwrap().is_empty(),
        "a slug lookup must not mint a by-name row"
    );

    // …while a plain (slug-free) unknown name still goes through
    // `CreateItemByName`, which is what makes `/Genres/{name}` never 404
    // upstream.
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Genres/BrandNew")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        created.lock().unwrap().as_slice(),
        &[(BaseItemKind::Genre, "BrandNew".to_owned())]
    );
}

#[tokio::test]
async fn music_genres_list_and_by_name() {
    assert_eq!(get("/MusicGenres").await.status(), StatusCode::OK);
    let response = get("/MusicGenres/Jazz").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["Name"], "Jazz");
    // Unlike the genres controller, this one keeps `if (item is null) return
    // NotFound()` on both trees — so a slug that resolves nothing is a 404
    // here and a 200 zero-DTO there. Measured against a live 10.11.8:
    // `/MusicGenres/R-B` J=404, `/Genres/R-B` J=200.
    assert_eq!(
        get("/MusicGenres/Nope-Nope").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn studios_list_and_by_name() {
    let list = get("/Studios").await;
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(json_body(list).await["Items"][0]["Name"], "A24");
    assert_eq!(get("/Studios/A24").await.status(), StatusCode::OK);
    // `StudiosController.GetStudio` is `CreateItemByName<Studio>` with no null
    // check in either tree, so an unseen studio name is materialized and
    // returned — never a 404.
    assert_eq!(get("/Studios/Nope").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn artists_list_album_artists_and_by_name() {
    assert_eq!(get("/Artists").await.status(), StatusCode::OK);
    assert_eq!(get("/Artists/AlbumArtists").await.status(), StatusCode::OK);
    let response = get("/Artists/Miles%20Davis").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["Name"], "Miles Davis");
}

/// The kind lists reach the repository as the client sent them: a Live TV kind
/// with no stored type name (`Program`) survives the lenient parse, so the
/// repository can answer it with nothing as `TranslateQuery` does, and a kind
/// both included and excluded arrives in both lists, for the repository to
/// apply the include alone (v12.2 `TranslateQuery.cs:282-313`).
#[tokio::test]
async fn by_name_lists_pass_the_kind_lists_through() {
    let library = ByNameLibrary::default();
    let state = by_name_state_with_library(library.clone());
    for route in [
        "/Genres",
        "/MusicGenres",
        "/Studios",
        "/Artists",
        "/Artists/AlbumArtists",
    ] {
        for (params, include, exclude) in [
            (
                "includeItemTypes=Program",
                vec![BaseItemKind::Program],
                vec![],
            ),
            (
                "includeItemTypes=Movie&excludeItemTypes=Movie",
                vec![BaseItemKind::Movie],
                vec![BaseItemKind::Movie],
            ),
        ] {
            let uri = format!("{route}?{params}");
            let response = create_router(state.clone())
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let seen = library.seen.lock().expect("lock");
            let query = seen.last().expect("a by-name query ran");
            assert_eq!(query.include_item_types, include, "{uri}");
            assert_eq!(query.exclude_item_types, exclude, "{uri}");
        }
    }
}

/// `studios` on the artist routes (v12.2 `ArtistsController.cs:182-195`,
/// `:336-349`): non-empty, the names' studio ids REPLACE `studioIds`. Upstream's
/// `GetStudio` mints a row for an unknown name, whose id nothing references;
/// here the lookup writes nothing and an unknown name keeps a no-match id, so a
/// list of unknown names never turns into no filter at all. The Genres and
/// Studios controllers declare no `studios`, so it never reaches their query.
/// The assertions are on the query the library receives: this fake answers
/// every list with one row, so the response's count says nothing here (the
/// repository's `studio_ids_naming_no_row_match_nothing` covers the empty
/// answer).
#[tokio::test]
async fn artist_lists_resolve_studio_names_without_creating_studios() {
    const OTHER_STUDIO: &str = "0000000000000000000000000000ab09";
    let other = Uuid::parse_str(OTHER_STUDIO).expect("uuid");
    let library = ByNameLibrary::default();
    let state = by_name_state_with_library(library.clone());
    let studio_ids_for = async |uri: String| {
        let response = create_router(state.clone())
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let seen = library.seen.lock().expect("lock");
        seen.last().expect("a by-name query ran").studio_ids.clone()
    };
    for route in ["/Artists", "/Artists/AlbumArtists"] {
        for (params, expected) in [
            (
                format!("studios=A24&studioIds={OTHER_STUDIO}"),
                vec![STUDIO_ID],
            ),
            ("studios=Nope".to_owned(), vec![Uuid::nil()]),
            (
                "studios=A24%7CNope".to_owned(),
                vec![STUDIO_ID, Uuid::nil()],
            ),
            // `|` alone names nothing, so `studioIds` stands.
            (format!("studioIds={OTHER_STUDIO}&studios=%7C"), vec![other]),
            // So does a whitespace-only value: the query layer binds it to null
            // for every member (`ConvertEmptyStringToNull`). Upstream's pipe
            // binder sees the raw value and keeps it as the name `""` instead
            // (gl-855).
            (format!("studioIds={OTHER_STUDIO}&studios=%20"), vec![other]),
            // A blank entry beside a name is dropped. Upstream keeps it as the
            // name `""`, an id that matches nothing, so it sends one more id
            // than the `[A24]` here; the artist routes answer the same.
            ("studios=A24%7C%20".to_owned(), vec![STUDIO_ID]),
        ] {
            let uri = format!("{route}?{params}");
            assert_eq!(studio_ids_for(uri.clone()).await, expected, "{uri}");
        }
    }
    for route in ["/Genres", "/MusicGenres", "/Studios"] {
        let ids = studio_ids_for(format!("{route}?studios=A24")).await;
        assert!(ids.is_empty(), "{route} took studios: {ids:?}");
    }
    assert!(
        library.created.lock().expect("lock").is_empty(),
        "a studios filter must not mint a studio row"
    );
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
async fn persons_filters_is_favorite_scopes_to_favorites() {
    // The Favorites screen sends `Filters=IsFavorite` (never `isFavorite=`);
    // the regression was the handler dropping the param and returning the
    // whole people table. Nobody is favorited here, so the result is empty.
    let response = get("/Persons?filters=IsFavorite").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Items"].as_array().map(Vec::len), Some(0));
    // An explicit `isFavorite` wins over the flag set (C# precedence).
    let response = get("/Persons?filters=IsFavorite&isFavorite=false").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["Items"][0]["Name"], "Uma");
}

#[tokio::test]
async fn years_list_and_by_value() {
    let list = get("/Years").await;
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(json_body(list).await["Items"][0]["Name"], "1999");
    let one = get("/Years/1999").await;
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(json_body(one).await["Name"], "1999");
    // An absent year is 404; a NON-POSITIVE one is 400, because C#
    // `LibraryManager.GetYear` throws `ArgumentOutOfRangeException("Years less
    // than or equal to 0 are invalid.")` before the controller's `NotFound()`
    // is ever reached (LibraryManager.cs:1020-1030). Measured on 10.11.8:
    // `/Years/0` and `/Years/-1` are 400 there.
    assert_eq!(get("/Years/0").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get("/Years/-1").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get("/Years/2020").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn by_name_routes_require_auth() {
    // With the default (rejecting) auth fake, the routes are 401 — proving they
    // exist and are guarded, not stubbed 501s.
    let router = create_router(ferrofin_api::test_support::fake_state());
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

/// A [`DtoService`] that records the [`DtoOptions`] each projection was handed.
///
/// The plain `OkDto` above ignores its `DtoOptions`, so it cannot observe the
/// thing these tests are about — which options the by-name handlers build from
/// the request. This one captures them.
#[derive(Default)]
struct RecordingDto {
    seen: std::sync::Mutex<Vec<DtoOptions>>,
}

impl RecordingDto {
    /// The options handed to the most recent projection call.
    fn last(&self) -> DtoOptions {
        self.seen
            .lock()
            .expect("recorder lock")
            .last()
            .cloned()
            .expect("a projection call was made")
    }
}

#[async_trait]
impl DtoService for RecordingDto {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dto(
        &self,
        item: &BaseItemEntity,
        o: &DtoOptions,
        _u: Option<&UserEntity>,
        _owner: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        self.seen.lock().expect("recorder lock").push(o.clone());
        Ok(entity_to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        o: &DtoOptions,
        _u: Option<&UserEntity>,
        _owner: Option<Uuid>,
        _skip: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        self.seen.lock().expect("recorder lock").push(o.clone());
        Ok(items.iter().map(entity_to_dto).collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        o: &DtoOptions,
        _tagged: Option<&[Uuid]>,
        _u: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        self.seen.lock().expect("recorder lock").push(o.clone());
        Ok(entity_to_dto(item))
    }
}

/// Drives `uri` through the by-name router with a recording DTO service and
/// returns the options that projection was given.
async fn options_for(uri: &str) -> DtoOptions {
    let recorder = Arc::new(RecordingDto::default());
    let state = by_name_state_with_dto(recorder.clone());
    let response = create_router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    recorder.last()
}

/// A by-name LIST must never project `ItemCounts`, even when the client asks
/// for it.
///
/// C# `DtoService.GetItemByNameDto` stamps the ten count fields only when
/// `ItemFields.ItemCounts` is requested AND `taggedItems` is non-empty:
///
/// ```csharp
/// if (options.ContainsField(ItemFields.ItemCounts)
///     && taggedItems is not null && taggedItems.Count != 0)
/// ```
///
/// Every by-name list passes `null` (Genres/MusicGenres/Studios/Artists via
/// `RequestHelpers.CreateQueryResult`, and Persons) or an empty list (Years),
/// so the guard never fires upstream — counts on a list come only from the
/// separate `includeItemTypes` block.
///
/// Ferrofin's `get_base_item_dtos` runs `name_counts_batch` on sight of
/// `ItemCounts` and stamps all ten (AlbumCount, ArtistCount, EpisodeCount,
/// MovieCount, MusicVideoCount, ProgramCount, SeriesCount, SongCount,
/// TrailerCount, ChildCount). That was unreachable while these routes
/// hardcoded "no fields"; honouring the caller's `fields` makes it reachable,
/// so the precondition is enforced in the projector instead.
#[tokio::test]
async fn a_by_name_list_never_projects_item_counts() {
    for uri in [
        "/Genres?fields=ItemCounts",
        "/MusicGenres?fields=ItemCounts",
        "/Studios?fields=ItemCounts",
        "/Artists?fields=ItemCounts",
        "/Persons?fields=ItemCounts",
        "/Years?fields=ItemCounts",
    ] {
        let opts = options_for(uri).await;
        assert!(
            !opts
                .fields
                .contains(&ferrofin_model::querying::ItemFields::ItemCounts),
            "{uri}: ItemCounts must not reach the by-name list projection — \
             Jellyfin's taggedItems guard is never satisfied for a list"
        );
    }

    // The client's OTHER fields must still get through — this strips one field,
    // it does not go back to ignoring the parameter.
    let opts = options_for("/Genres?fields=ItemCounts,Path").await;
    assert!(
        opts.fields
            .contains(&ferrofin_model::querying::ItemFields::Path),
        "stripping ItemCounts must not discard the rest of `fields`"
    );
}

#[tokio::test]
async fn genres_browse_never_enables_user_data() {
    // `GenresController.GetGenres` passes a literal `false` for `enableUserData`
    // to `AddAdditionalDtoOptions`, so Jellyfin never emits a `UserData` block on
    // a genre row. Ferrofin used to send one on every genre and music genre.
    assert!(!options_for("/Genres").await.enable_user_data);
    assert!(!options_for("/MusicGenres").await.enable_user_data);
    // Even an explicit request cannot turn it on — upstream has no such param.
    assert!(
        !options_for("/Genres?enableUserData=true")
            .await
            .enable_user_data
    );
}

#[tokio::test]
async fn by_name_browses_honour_requested_fields() {
    use ferrofin_model::querying::ItemFields;
    for uri in [
        "/Genres?fields=Overview,Path",
        "/MusicGenres?fields=Overview,Path",
        "/Studios?fields=Overview,Path",
        "/Artists?fields=Overview,Path",
        "/Artists/AlbumArtists?fields=Overview,Path",
        "/Persons?fields=Overview,Path",
        "/Years?fields=Overview,Path",
    ] {
        let o = options_for(uri).await;
        assert!(o.contains_field(ItemFields::Overview), "{uri}");
        assert!(o.contains_field(ItemFields::Path), "{uri}");
        assert!(
            !o.contains_field(ItemFields::People),
            "{uri} over-populates"
        );
    }
    // Absent `fields` ⇒ the base DTO, as `new DtoOptions { Fields = [] }`.
    let fields = options_for("/Genres").await.fields;
    assert!(fields.is_empty(), "{fields:?}");
}

#[tokio::test]
async fn by_name_browses_honour_image_and_user_data_toggles() {
    use ferrofin_model::entities::ImageType;
    let o =
        options_for("/Studios?imageTypeLimit=1&enableImageTypes=Primary&enableImages=false").await;
    assert_eq!(o.image_type_limit, 1);
    assert_eq!(o.image_types, vec![ImageType::Primary]);
    assert!(!o.enable_images);
    // `enableUserData` is forwarded on the controllers that expose it.
    assert!(
        !options_for("/Studios?enableUserData=false")
            .await
            .enable_user_data
    );
    assert!(options_for("/Studios").await.enable_user_data);
    assert!(
        !options_for("/Persons?enableUserData=false")
            .await
            .enable_user_data
    );
    assert!(
        !options_for("/Years?enableUserData=false")
            .await
            .enable_user_data
    );
    assert!(
        !options_for("/Artists?enableUserData=false")
            .await
            .enable_user_data
    );
    // Defaults with no toggles sent: images on, no per-type limit.
    let base = options_for("/Studios").await;
    assert!(base.enable_images);
    assert_eq!(base.image_type_limit, i32::MAX);
}

/// An API key that names no `userId` has no user, and every by-name list runs
/// with none, unscoped: v12.2 `GenresController.GetGenres` and its siblings
/// build `new InternalItemsQuery(user)` with `user` null
/// (`GenresController.cs:106-112`, `PersonsController.cs:98-118`,
/// `YearsController.cs:95-100`), and `/Persons` builds no access filter
/// (`BuildAccessFilter(null)`). Never the `400` of a request with no user.
#[rstest::rstest]
#[case::genres("/Genres")]
#[case::music_genres("/MusicGenres")]
#[case::studios("/Studios")]
#[case::artists("/Artists")]
#[case::album_artists("/Artists/AlbumArtists")]
#[case::persons("/Persons")]
#[case::years("/Years")]
#[tokio::test]
async fn an_api_key_lists_by_name_items_with_no_user(#[case] uri: &str) {
    let library = ByNameLibrary::default();
    let state = by_name_state_authed(Arc::new(OkDto), library.clone(), Arc::new(ApiKeyAuth));
    let response = create_router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let body = json_body(response).await;
    assert_eq!(body["TotalRecordCount"], 1, "{uri}");
    let people = library.seen_people.lock().expect("lock");
    let items = library.seen.lock().expect("lock");
    assert!(
        !people.is_empty() || !items.is_empty(),
        "{uri} ran no query"
    );
    for query in people.iter() {
        assert!(query.user_id.is_none(), "{uri} ran as a user");
        assert!(
            query.access_filter.is_none(),
            "{uri} built an access filter"
        );
    }
    for query in items.iter() {
        assert!(query.user.is_none(), "{uri} ran as a user");
    }
}

/// The single by-name routes serve an API key with no user too: v12.2
/// `GetGenre`, `GetMusicGenre`, `GetStudio`, `GetArtistByName`, `GetPerson`
/// and `GetYear` project the item with no user when `userId` is empty.
#[rstest::rstest]
#[case::genre("/Genres/Drama", "Drama")]
#[case::music_genre("/MusicGenres/Jazz", "Jazz")]
#[case::studio("/Studios/A24", "A24")]
#[case::artist("/Artists/Miles%20Davis", "Miles Davis")]
#[case::person("/Persons/Uma", "Uma")]
#[case::year("/Years/1999", "1999")]
#[tokio::test]
async fn an_api_key_reads_a_by_name_item_with_no_user(#[case] uri: &str, #[case] name: &str) {
    let state = by_name_state_authed(
        Arc::new(OkDto),
        ByNameLibrary::default(),
        Arc::new(ApiKeyAuth),
    );
    let response = create_router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    assert_eq!(json_body(response).await["Name"], name, "{uri}");
}

/// v12.2 `LibraryManager.GetParentItem` (`LibraryManager.cs:3276-3289`), which
/// every by-name list calls, throws `ArgumentException("Invalid parent id: …")`
/// for a `parentId` naming no item, the zero id included (`GetItemById`), and
/// `ExceptionMiddleware` answers it `400`; nothing is listed. A parent that
/// exists still scopes the list.
#[rstest::rstest]
#[case::genres("/Genres")]
#[case::music_genres("/MusicGenres")]
#[case::studios("/Studios")]
#[case::artists("/Artists")]
#[case::album_artists("/Artists/AlbumArtists")]
#[case::years("/Years")]
#[tokio::test]
async fn a_by_name_list_refuses_a_parent_naming_no_item(#[case] route: &str) {
    for parent in [
        Uuid::from_u128(0xBBFF).simple().to_string(),
        Uuid::nil().to_string(),
    ] {
        let library = ByNameLibrary::default();
        let response = create_router(by_name_state_with_library(library.clone()))
            .oneshot(
                Request::builder()
                    .uri(format!("{route}?parentId={parent}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{route} {parent}"
        );
        assert!(
            library.seen.lock().expect("lock").is_empty(),
            "{route} {parent}: nothing is listed"
        );
    }

    let library = ByNameLibrary::default();
    let response = create_router(by_name_state_with_library(library.clone()))
        .oneshot(
            Request::builder()
                .uri(format!("{route}?parentId={}", MOVIES_LIBRARY_ID.simple()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{route} under a library");
    let seen = library.seen.lock().expect("lock");
    let query = seen.last().expect("the list ran");
    if route == "/Years" {
        assert_eq!(
            query.parent_id, MOVIES_LIBRARY_ID,
            "{route} scoped by its parent"
        );
    } else {
        assert_eq!(
            query.ancestor_ids,
            [MOVIES_LIBRARY_ID],
            "{route} scoped by its parent"
        );
    }
}
