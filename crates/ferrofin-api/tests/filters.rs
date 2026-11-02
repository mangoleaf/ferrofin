//! Item-filters handler tests: `/Items/Filters` (legacy) and `/Items/Filters2`.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned facets,
//! asserting the success status and the wire-body shape. Managers a given handler
//! never touches reuse the `test_support` panic fakes, catching a handler that
//! strays.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAppHost, FakeConfig, FakeMediaSources, FakeProviders, FakeSessions, FakeSystem,
    FakeUserData, FakeUserViews,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::{BaseItemDto, NameGuidPair, NameValuePair, RecommendationType};
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFilters, QueryFiltersLegacy, QueryResult};
use ferrofin_model::search::{SearchHint, SearchQuery};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    LibraryManager, MusicManager, SearchManager, SearchResult, SimilarItemsManager,
    SimilarItemsRecommendation, UserManager,
};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::persistence::ItemWithCounts;
use rstest::rstest;
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
        normalized_username: username.to_uppercase(),
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

/// A [`LibraryManager`] backing the filters/filters2 facet routes; unused methods
/// panic.
///
/// It also **records** the [`InternalItemsQuery`] each facet call is handed. That
/// query is the handler's own output (the facet lists it returns are canned), so
/// asserting on it tests the code under test rather than the stub.
#[derive(Default)]
struct StubLibrary {
    /// The last query passed to [`LibraryManager::get_genres`].
    last_genre_query: Mutex<Option<InternalItemsQuery>>,
    /// When set, [`LibraryManager::get_genres`] also returns a row whose stored
    /// `Id` is not a Guid (a corrupt/adopted row) — the case the handler must
    /// drop rather than publish as the nil GUID.
    malformed_genre_id: bool,
    /// The last query passed to [`LibraryManager::get_media_stream_languages`]
    /// (the handler's stream-language query).
    last_stream_query: Mutex<Option<InternalItemsQuery>>,
    /// Overrides the canned `(audio, subtitle)` language codes.
    stream_languages: Option<(Vec<&'static str>, Vec<&'static str>)>,
    /// An extra item [`LibraryManager::get_item_by_id`] knows, stored as the
    /// given kind: the `parentId` a test passes.
    parent: Option<(Uuid, BaseItemKind)>,
    /// The last query passed to [`LibraryManager::get_query_filters_legacy`],
    /// which the trait's default `get_tag_names` (the Filters2 tag facet)
    /// reads.
    last_legacy_query: Mutex<Option<InternalItemsQuery>>,
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
        if let Some((parent, kind)) = self.parent
            && parent == id
        {
            let mut item = item_entity(parent, "Parent");
            kind.stored_type_name()
                .expect("a stored kind")
                .clone_into(&mut item.type_);
            return Ok(Some(item));
        }
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
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        *self.last_genre_query.lock().expect("genre query lock") = Some(query.clone());
        let mut items = vec![ItemWithCounts {
            item: item_entity(Uuid::from_u128(0x21), "Action"),
            counts: ferrofin_model::dto::ItemCounts::default(),
        }];
        if self.malformed_genre_id {
            let mut item = item_entity(Uuid::from_u128(0x23), "Corrupt");
            "not-a-guid".clone_into(&mut item.id);
            items.push(ItemWithCounts {
                item,
                counts: ferrofin_model::dto::ItemCounts::default(),
            });
        }
        let total = i32::try_from(items.len()).expect("small");
        Ok(QueryResult::new(Some(0), Some(total), items))
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
                counts: ferrofin_model::dto::ItemCounts::default(),
            }],
        ))
    }
    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        *self.last_legacy_query.lock().expect("legacy query lock") = Some(query.clone());
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
        query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        *self.last_stream_query.lock().expect("stream query lock") = Some(query.clone());
        // Deliberately not in `Name` order: the handler sorts.
        let (audio, subtitle) = self
            .stream_languages
            .clone()
            .unwrap_or_else(|| (vec!["deu", "eng"], vec!["fra"]));
        Ok(match stream_type {
            MediaStreamType::Audio => audio,
            MediaStreamType::Subtitle => subtitle,
            _ => Vec::new(),
        }
        .into_iter()
        .map(str::to_owned)
        .collect())
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
        _c: ferrofin_model::data::CollectionType,
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
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
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
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
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

/// A culture table for the language facets: `FindLanguageInfo` knows `eng`,
/// `deu`, `fra`, `eus` and `zxx` (with upstream's raw, untruncated English
/// names); every other code is unknown. Everything else is the INFRA fake.
struct LanguageNames;

impl ferrofin_traits::localization::LocalizationManager for LanguageNames {
    fn get_cultures(&self) -> Vec<ferrofin_model::globalization::CultureDto> {
        ferrofin_api::test_support::FakeLocalization.get_cultures()
    }
    fn get_countries(&self) -> Vec<ferrofin_model::globalization::CountryInfo> {
        ferrofin_api::test_support::FakeLocalization.get_countries()
    }
    fn get_parental_ratings(&self) -> Vec<ferrofin_model::entities_media::ParentalRating> {
        ferrofin_api::test_support::FakeLocalization.get_parental_ratings()
    }
    fn get_localization_options(&self) -> Vec<ferrofin_model::globalization::LocalizationOption> {
        ferrofin_api::test_support::FakeLocalization.get_localization_options()
    }
    fn get_supported_ui_cultures(&self) -> Vec<String> {
        ferrofin_api::test_support::FakeLocalization.get_supported_ui_cultures()
    }
    fn get_localized_string(&self, phrase: &str) -> String {
        phrase.to_owned()
    }
    fn get_server_localized_string(&self, phrase: &str) -> String {
        phrase.to_owned()
    }
    fn get_localized_string_for(&self, phrase: &str, _culture: &str) -> String {
        phrase.to_owned()
    }
    fn find_language_info(
        &self,
        language: &str,
    ) -> Option<ferrofin_model::globalization::CultureDto> {
        let display_name = match language {
            "eng" => "English",
            "deu" => "German",
            "fra" => "French",
            "eus" => "Basque",
            "zxx" => "No linguistic content; Not applicable",
            _ => return None,
        };
        Some(ferrofin_model::globalization::CultureDto {
            name: display_name.to_owned(),
            display_name: display_name.to_owned(),
            two_letter_iso_language_name: String::new(),
            three_letter_iso_language_name: Some(language.to_owned()),
            three_letter_iso_language_names: vec![language.to_owned()],
        })
    }
    fn get_language_display_name(&self, _language: &str) -> Option<String> {
        // The facet names must come from `find_language_info`; this one
        // truncates at `;`, so a handler reaching for it gets nothing.
        None
    }
    fn get_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ferrofin_model::entities_media::ParentalRatingScore> {
        ferrofin_api::test_support::FakeLocalization.get_rating_score(rating, country_code)
    }
}

/// Builds an [`AppState`] wired with the filter stubs around `library`.
fn state_with(library: Arc<StubLibrary>) -> AppState {
    AppState::new(
        library,
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
        Arc::new(LanguageNames),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
}

/// Drives one GET request through a router built over `library`, returning
/// (status, body bytes).
async fn get_with(library: Arc<StubLibrary>, uri: &str) -> (StatusCode, Vec<u8>) {
    let router = create_router(state_with(library));
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

/// [`get_with`] over a throwaway library stub.
async fn get(uri: &str) -> (StatusCode, Vec<u8>) {
    get_with(Arc::new(StubLibrary::default()), uri).await
}

#[tokio::test]
async fn items_filters_returns_legacy_facets() {
    let (status, body) = get("/Items/Filters?includeItemTypes=Movie").await;
    assert_eq!(status, StatusCode::OK);
    let filters: QueryFiltersLegacy = serde_json::from_slice(&body).expect("legacy filters");
    assert_eq!(filters.genres, vec!["Action".to_owned()]);
    assert_eq!(filters.years, vec![1999]);
}

#[tokio::test]
async fn items_filters2_returns_genre_facets() {
    let (status, body) = get("/Items/Filters2?includeItemTypes=Movie").await;
    assert_eq!(status, StatusCode::OK);
    let filters: QueryFilters = serde_json::from_slice(&body).expect("filters");
    assert_eq!(
        filters.genres,
        vec![NameGuidPair {
            name: Some("Action".to_owned()),
            id: Uuid::from_u128(0x21),
        }]
    );
    // The response schema is exactly Jellyfin 12's QueryFilters, in class
    // order: {Genres, Tags, AudioLanguages, SubtitleLanguages}.
    assert_eq!(
        body_keys(&body),
        ["Genres", "Tags", "AudioLanguages", "SubtitleLanguages"]
    );
}

/// The top-level keys of a JSON object body, in wire order.
fn body_keys(body: &[u8]) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(body).expect("json");
    let mut keys: Vec<(usize, String)> = value
        .as_object()
        .expect("object")
        .keys()
        .map(|key| {
            let needle = format!("\"{key}\":");
            let text = std::str::from_utf8(body).expect("utf-8");
            (text.find(&needle).expect("key in body"), key.clone())
        })
        .collect();
    keys.sort();
    keys.into_iter().map(|(_, key)| key).collect()
}

/// `{Name, Value}` facet pairs, in order.
fn pairs(pairs: &[(&str, &str)]) -> Vec<NameValuePair> {
    pairs
        .iter()
        .map(|&(name, value)| NameValuePair::new(name, value))
        .collect()
}

/// `GET /Items/Filters2` over `library`, decoded.
async fn filters2(library: Arc<StubLibrary>, uri: &str) -> QueryFilters {
    let (status, body) = get_with(library, uri).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).expect("filters")
}

#[tokio::test]
async fn items_filters2_names_languages_and_sorts_them_by_name() {
    // v12.2 `FilterController.cs:246-271`: `"{DisplayName} ({code})"`, then
    // `OrderBy(l => l.Name)` — the stub hands back `deu` before `eng`.
    let filters = filters2(
        Arc::new(StubLibrary::default()),
        "/Items/Filters2?includeItemTypes=Movie",
    )
    .await;
    assert_eq!(
        filters.audio_languages,
        pairs(&[("English (eng)", "eng"), ("German (deu)", "deu")])
    );
    assert_eq!(
        filters.subtitle_languages,
        pairs(&[("French (fra)", "fra")])
    );
}

#[tokio::test]
async fn items_filters2_keeps_unknown_codes_bare_and_display_names_whole() {
    // An unknown code names itself; a known one uses the raw iso6392.txt name
    // — `FindLanguageInfo(...).DisplayName`, not the `;`-truncating
    // `GetLanguageDisplayName`.
    let library = Arc::new(StubLibrary {
        stream_languages: Some((vec!["xyz", "zxx"], vec!["und"])),
        ..StubLibrary::default()
    });
    let filters = filters2(library, "/Items/Filters2?includeItemTypes=Movie").await;
    assert_eq!(
        filters.audio_languages,
        pairs(&[
            ("No linguistic content; Not applicable (zxx)", "zxx"),
            ("xyz", "xyz"),
        ])
    );
    assert_eq!(filters.subtitle_languages, pairs(&[("und", "und")]));
}

#[tokio::test]
async fn items_filters2_sorts_languages_linguistically_not_ordinally() {
    // `Comparer<string>.Default` is the culture comparer: "abc" sorts before
    // "Basque (eus)", where an ordinal sort puts 'B' (0x42) before 'a' (0x61).
    let library = Arc::new(StubLibrary {
        stream_languages: Some((vec!["eus", "abc"], Vec::new())),
        ..StubLibrary::default()
    });
    let filters = filters2(library, "/Items/Filters2?includeItemTypes=Movie").await;
    assert_eq!(
        filters.audio_languages,
        pairs(&[("abc", "abc"), ("Basque (eus)", "eus")])
    );
    assert!(filters.subtitle_languages.is_empty());
}

/// Which type sets get the language facets (v12.2 `FilterController.cs:242-245`)
/// and which kinds the stream query then runs over (`:212-217`: a show or
/// season set gains `Episode`, appended last). `None` = no stream query, and
/// both facets stay `[]`.
#[rstest]
#[case("Movie", Some(vec![BaseItemKind::Movie]))]
#[case("Series", Some(vec![BaseItemKind::Series, BaseItemKind::Episode]))]
#[case("Season", Some(vec![BaseItemKind::Season, BaseItemKind::Episode]))]
#[case("Series,Episode", Some(vec![BaseItemKind::Series, BaseItemKind::Episode]))]
#[case("Episode", Some(vec![BaseItemKind::Episode]))]
#[case(
    "Movie,Season",
    Some(vec![BaseItemKind::Movie, BaseItemKind::Season, BaseItemKind::Episode])
)]
#[case("Audio", None)]
#[case("BoxSet", None)]
#[case("", None)]
#[tokio::test]
async fn items_filters2_language_facets_gate_and_widen_by_type(
    #[case] types: &str,
    #[case] expected: Option<Vec<BaseItemKind>>,
) {
    let library = Arc::new(StubLibrary::default());
    let uri = if types.is_empty() {
        "/Items/Filters2".to_owned()
    } else {
        format!("/Items/Filters2?includeItemTypes={types}")
    };
    let filters = filters2(Arc::clone(&library), &uri).await;
    let stream_query = library
        .last_stream_query
        .lock()
        .expect("stream query lock")
        .clone();
    if let Some(kinds) = expected {
        let query = stream_query.expect("stream languages queried");
        assert_eq!(query.include_item_types, kinds);
        assert!(query.include_owned_items, "owned items are included");
        assert!(!filters.audio_languages.is_empty());
    } else {
        assert!(stream_query.is_none(), "no stream query for {types:?}");
        assert!(filters.audio_languages.is_empty());
        assert!(filters.subtitle_languages.is_empty());
    }
}

#[rstest]
#[case("true")]
#[case("false")]
#[tokio::test]
async fn items_filters2_stream_query_shares_the_genre_query_scope(#[case] recursive: &str) {
    // `:198-210`: one scoping branch sets the genre and the stream query alike,
    // and both carry the user and the live-TV `Is*` flags (`:161-178`).
    let parent = Uuid::from_u128(0xF00D);
    let library = Arc::new(StubLibrary {
        parent: Some((parent, BaseItemKind::Folder)),
        ..StubLibrary::default()
    });
    filters2(
        Arc::clone(&library),
        &format!(
            "/Items/Filters2?userId={USER_ID}&includeItemTypes=Movie&parentId={parent}\
             &isMovie=true&recursive={recursive}"
        ),
    )
    .await;
    let stream = library
        .last_stream_query
        .lock()
        .expect("stream query lock")
        .clone()
        .expect("stream languages queried");
    let genre = library
        .last_genre_query
        .lock()
        .expect("genre query lock")
        .clone()
        .expect("get_genres called");
    if recursive == "true" {
        assert_eq!(stream.ancestor_ids, vec![parent]);
    } else {
        assert_eq!(stream.parent_id, parent);
        assert!(stream.ancestor_ids.is_empty());
    }
    assert_eq!(stream.ancestor_ids, genre.ancestor_ids);
    assert_eq!(stream.parent_id, genre.parent_id);
    assert_eq!(stream.is_movie, Some(true));
    assert_eq!(stream.user_id(), Some(USER_ID));
    // Only the two upstream differences separate them.
    assert!(!genre.include_owned_items);
    assert_eq!(
        InternalItemsQuery {
            include_owned_items: false,
            ..stream
        },
        genre
    );
}

#[tokio::test]
async fn items_filters_legacy_carries_no_language_facets() {
    // The languages moved to QueryFilters in Jellyfin 12; the legacy set is
    // still {Genres, Tags, OfficialRatings, Years}.
    let (status, body) = get("/Items/Filters?includeItemTypes=Movie").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body_keys(&body),
        ["Genres", "Tags", "OfficialRatings", "Years"]
    );
}

#[tokio::test]
async fn items_filters2_drops_a_genre_whose_stored_id_is_not_a_guid() {
    // `Id` is what the client sends back as `genreIds=…`. Upstream reads a Guid
    // column and can only emit real ids; Ferrofin stores the id as text, so a row
    // that will not parse is dropped rather than published as the nil GUID — a
    // facet chip that resolves to nothing is worse than no chip.
    let library = Arc::new(StubLibrary {
        malformed_genre_id: true,
        ..StubLibrary::default()
    });
    let (status, body) = get_with(library, "/Items/Filters2?includeItemTypes=Movie").await;
    assert_eq!(status, StatusCode::OK);
    let filters: QueryFilters = serde_json::from_slice(&body).expect("filters");
    assert_eq!(
        filters.genres,
        vec![NameGuidPair {
            name: Some("Action".to_owned()),
            id: Uuid::from_u128(0x21),
        }],
        "only the well-formed facet is published"
    );
    assert!(
        !filters.genres.iter().any(|pair| pair.id.is_nil()),
        "no nil-GUID facet reaches the client"
    );
}

#[tokio::test]
async fn items_filters2_music_type_uses_music_genres() {
    let (status, body) = get("/Items/Filters2?includeItemTypes=Audio").await;
    assert_eq!(status, StatusCode::OK);
    let filters: QueryFilters = serde_json::from_slice(&body).expect("filters");
    assert_eq!(
        filters.genres.first().and_then(|p| p.name.clone()),
        Some("Jazz".to_owned())
    );
}

#[tokio::test]
async fn items_filters2_fills_tags_from_the_tag_names() {
    // Jellyfin 12.1+ `filters.Tags = _libraryManager.GetTagNames(tagQuery)`
    // (v12.2 `FilterController.cs:240`). The stub answers tags only through its
    // legacy facet set, which the trait's default `get_tag_names` reads.
    // It runs for every type set: music, and a lone Trailer, included.
    for types in ["Movie", "Audio", "Trailer"] {
        let (status, body) = get(&format!("/Items/Filters2?includeItemTypes={types}")).await;
        assert_eq!(status, StatusCode::OK);
        let filters: QueryFilters = serde_json::from_slice(&body).expect("filters");
        assert_eq!(filters.tags, vec!["Cult".to_owned()], "{types}");
    }
}

#[tokio::test]
async fn items_filters2_tag_query_is_the_genre_querys_twin() {
    // `tagQuery` (v12.2 `FilterController.cs:181-196`) takes the genre query's
    // user, types and live-TV flags, and the same parent scope (`:198-210`):
    // unlike the stream-language query it neither includes owned items nor
    // widens a show set to its episodes.
    let parent = Uuid::from_u128(0xF00D);
    let library = Arc::new(StubLibrary {
        parent: Some((parent, BaseItemKind::Folder)),
        ..StubLibrary::default()
    });
    let (status, _) = get_with(
        Arc::clone(&library),
        &format!(
            "/Items/Filters2?userId={USER_ID}&includeItemTypes=Series&parentId={parent}\
             &isSeries=true&recursive=false"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tag = library
        .last_legacy_query
        .lock()
        .expect("tag query lock")
        .clone()
        .expect("tag names read");
    let genre = library
        .last_genre_query
        .lock()
        .expect("genre query lock")
        .clone()
        .expect("get_genres called");
    assert_eq!(tag, genre);
    assert_eq!(tag.include_item_types, vec![BaseItemKind::Series]);
    assert!(!tag.include_owned_items);
    assert_eq!(tag.parent_id, parent);
    assert_eq!(tag.user_id(), Some(USER_ID));
}

/// Runs `/Items/Filters2` over a parent of `kind` (or an unknown parent when
/// `None`) and returns the genre and stream-language queries the handler built.
async fn filters2_queries(
    kind: Option<BaseItemKind>,
    query_suffix: &str,
) -> (Uuid, InternalItemsQuery, InternalItemsQuery) {
    let parent = Uuid::from_u128(0xF00D);
    let library = Arc::new(StubLibrary {
        parent: kind.map(|kind| (parent, kind)),
        ..StubLibrary::default()
    });
    let (status, _) = get_with(
        Arc::clone(&library),
        &format!("/Items/Filters2?includeItemTypes=Movie&parentId={parent}{query_suffix}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let genre = library
        .last_genre_query
        .lock()
        .expect("genre query lock")
        .clone()
        .expect("get_genres called");
    let stream = library
        .last_stream_query
        .lock()
        .expect("stream query lock")
        .clone()
        .expect("stream languages read");
    (parent, genre, stream)
}

#[rstest]
// `(recursive ?? true) || parentItem is UserView || parentItem is
// ICollectionFolder` (v12.2 `FilterController.cs:198`) takes the ancestor
// scope. `BasePluginFolder` (the playlists folder) is an `ICollectionFolder`.
#[case::recursive_folder(BaseItemKind::Folder, "&recursive=true")]
#[case::default_recursive(BaseItemKind::Folder, "")]
#[case::library(BaseItemKind::CollectionFolder, "&recursive=false")]
#[case::view(BaseItemKind::UserView, "&recursive=false")]
#[case::playlists_folder(BaseItemKind::PlaylistsFolder, "&recursive=false")]
#[case::manual_playlists_folder(BaseItemKind::ManualPlaylistsFolder, "&recursive=false")]
#[case::plugin_folder(BaseItemKind::BasePluginFolder, "&recursive=false")]
#[tokio::test]
async fn items_filters2_scopes_to_the_parents_descendants(
    #[case] kind: BaseItemKind,
    #[case] query_suffix: &str,
) {
    let (parent, genre, stream) = filters2_queries(Some(kind), query_suffix).await;
    for query in [&genre, &stream] {
        assert_eq!(query.ancestor_ids, vec![parent]);
        assert_eq!(query.parent_id, Uuid::nil());
        assert!(!query.physical_children_only);
    }
}

#[rstest]
#[case::folder(BaseItemKind::Folder)]
#[case::series(BaseItemKind::Series)]
#[case::box_set(BaseItemKind::BoxSet)]
// Plain `Folder` subclasses upstream, not an `ICollectionFolder`.
#[case::aggregate_folder(BaseItemKind::AggregateFolder)]
#[case::user_root_folder(BaseItemKind::UserRootFolder)]
#[case::channel(BaseItemKind::Channel)]
#[tokio::test]
async fn items_filters2_non_recursive_scopes_to_the_parents_children(#[case] kind: BaseItemKind) {
    // The else branch: `genreQuery.Parent = parentItem` (and the stream and tag
    // queries alike), matched as a strict `ParentId`: the direct children only.
    let (parent, genre, stream) = filters2_queries(Some(kind), "&recursive=false").await;
    for query in [&genre, &stream] {
        assert_eq!(query.parent_id, parent);
        assert!(query.ancestor_ids.is_empty(), "{:?}", query.ancestor_ids);
        assert!(query.physical_children_only);
    }
}

#[rstest]
#[case::recursive("&recursive=true")]
#[case::non_recursive("&recursive=false")]
#[tokio::test]
async fn items_filters2_unknown_parent_scopes_to_nothing(#[case] query_suffix: &str) {
    // `GetItemById` finds no item, so `parentItem` is null: the ancestor branch
    // binds `Array.Empty<Guid>()` and the else branch `Parent = null`. Both
    // leave the facets over everything the user can see.
    let (_, genre, stream) = filters2_queries(None, query_suffix).await;
    for query in [&genre, &stream] {
        assert!(query.ancestor_ids.is_empty(), "{:?}", query.ancestor_ids);
        assert_eq!(query.parent_id, Uuid::nil());
    }
}

#[rstest]
#[case::trailer("Trailer", "")]
#[case::program("Program", "")]
#[case::non_recursive_program("Program", "&recursive=false")]
#[tokio::test]
async fn items_filters2_trailer_type_skips_the_parent(
    #[case] kind: &str,
    #[case] query_suffix: &str,
) {
    // C#: a lone Trailer/Program type set forces `parentItem = null`, so no
    // scope is set even when `parentId` names a folder that exists.
    let parent = Uuid::from_u128(0xF00D);
    let library = Arc::new(StubLibrary {
        parent: Some((parent, BaseItemKind::Folder)),
        ..StubLibrary::default()
    });
    let (status, _) = get_with(
        Arc::clone(&library),
        &format!("/Items/Filters2?includeItemTypes={kind}&parentId={parent}{query_suffix}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let query = library
        .last_genre_query
        .lock()
        .expect("genre query lock")
        .clone()
        .expect("get_genres called");
    assert!(query.ancestor_ids.is_empty());
    assert_eq!(query.parent_id, Uuid::nil());
    assert!(!query.physical_children_only);
}

/// v12.2 `FilterController.GetQueryFiltersLegacy` (`FilterController.cs:66-77`)
/// resolves the parent through `LibraryManager.GetParentItem`, which throws
/// `ArgumentException("Invalid parent id: …")` for an id naming no item;
/// `ExceptionMiddleware` answers it `400`. A parent that exists is the scope.
#[tokio::test]
async fn items_filters_refuses_an_unknown_parent() {
    for unknown in [Uuid::from_u128(0xBAD0), Uuid::nil()] {
        let library = Arc::new(StubLibrary::default());
        let (status, body) = get_with(
            Arc::clone(&library),
            &format!("/Items/Filters?includeItemTypes=Movie&parentId={unknown}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{unknown}");
        // `GetItemById` throws "Guid can't be empty" for the zero id.
        let message = if unknown.is_nil() {
            "Guid can't be empty"
        } else {
            "Invalid parent id"
        };
        assert!(
            String::from_utf8_lossy(&body).contains(message),
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            library.last_legacy_query.lock().expect("lock").is_none(),
            "{unknown}: nothing is aggregated for an unknown parent"
        );
    }

    let parent = Uuid::from_u128(0xF00D);
    let library = Arc::new(StubLibrary {
        parent: Some((parent, BaseItemKind::Folder)),
        ..StubLibrary::default()
    });
    let (status, _) = get_with(
        Arc::clone(&library),
        &format!("/Items/Filters?includeItemTypes=Movie&parentId={parent}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let query = library
        .last_legacy_query
        .lock()
        .expect("lock")
        .clone()
        .expect("aggregated");
    assert_eq!(query.ancestor_ids, vec![parent]);
}

/// A lone `Trailer` or `Program` type leaves `GetQueryFiltersLegacy`'s item
/// null (`FilterController.cs:66-72`), which is no `Folder`, so upstream
/// answers empty filters whatever `parentId` says, an unknown one included.
#[rstest]
#[case::trailer("Trailer")]
#[case::program("Program")]
#[tokio::test]
async fn items_filters_for_a_lone_trailer_or_program_type_are_empty(#[case] kind: &str) {
    for parent in ["", "&parentId=0000000000000000000000000000bad0"] {
        let library = Arc::new(StubLibrary::default());
        let (status, body) = get_with(
            Arc::clone(&library),
            &format!("/Items/Filters?includeItemTypes={kind}{parent}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{kind}{parent}");
        let filters: QueryFiltersLegacy = serde_json::from_slice(&body).expect("legacy filters");
        assert_eq!(filters, QueryFiltersLegacy::default(), "{kind}{parent}");
        assert!(
            library.last_legacy_query.lock().expect("lock").is_none(),
            "{kind}{parent}: nothing is aggregated"
        );
    }
}

/// `/Items/Filters2` reads its parent through `GetItemById<BaseItem>`, which
/// answers null for an id naming no item (no scope) but throws for the zero
/// id ("Guid can't be empty"), which `ExceptionMiddleware` answers `400`
/// (v12.2 `FilterController.cs:138-141`, `LibraryManager.cs:1689-1694`).
#[tokio::test]
async fn items_filters2_refuses_the_zero_parent_id() {
    let (status, _) = get(&format!(
        "/Items/Filters2?includeItemTypes=Movie&parentId={}",
        Uuid::nil()
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&format!(
        "/Items/Filters2?includeItemTypes=Movie&parentId={}",
        Uuid::from_u128(0xBAD0)
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "an unknown id is no scope");
}
