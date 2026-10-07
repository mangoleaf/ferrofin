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
use ferrofin_model::entities::MetadataField;
use ferrofin_model::providers::ExternalIdInfo;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, ScanTarget, UserManager};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::providers::{
    ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
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
        normalized_username: username.to_uppercase(),
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
        type_: "Movie".to_owned(),
        unrated_type: None,
        width: None,
    }
}

/// An [`AuthService`]/[`AuthorizationContext`] that authenticates as [`USER_ID`].
/// Every route this file covers — `POST /Items/{itemId}`, `ContentType`,
/// `MetadataEditor`, `Refresh`, `ExternalIdInfos` — is `RequiresElevation` upstream,
/// so this stub authenticates as an API key — which satisfies the policy
/// without a user/policy lookup, exactly as C# does. The gate itself is pinned
/// end to end in `apps/ferrofin-server/tests/elevation.rs`.
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            is_api_key: true,
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
            is_api_key: true,
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

/// The external-id writes a test captures: `(item id, the whole new set)` per call
/// to `update_item_provider_ids`.
type RecordedProviderIds = Arc<Mutex<Vec<(Uuid, Vec<(String, String)>)>>>;

/// A [`LibraryManager`] resolving a single known item id (any other is `None`);
/// `update_items` succeeds so the edit handler runs end-to-end. The folder
/// fields shape the resolved entity for the folder-refresh (refresh scan)
/// tests, which assert against `refresh_scans`.
struct OkLibrary {
    item_id: Uuid,
    is_folder: bool,
    top_parent_id: Option<Uuid>,
    refresh_scans: RecordedScans,
    /// Entities passed to `update_items`, for asserting what the edit wrote.
    updated: Arc<Mutex<Vec<BaseItemEntity>>>,
    /// External-id sets passed to `update_item_provider_ids` — a second write,
    /// because `BaseItemProviders` is its own table.
    provider_ids: RecordedProviderIds,
    /// The item's children and locked fields, for the cascade tests.
    tree: Arc<Tree>,
}

/// What the cascade tests shape around the fixture item: its stored row, its
/// children by parent, its descendants, and every item's `LockedFields`.
#[derive(Default)]
struct Tree {
    /// Replaces the fixture item's stored row when set.
    root: Option<BaseItemEntity>,
    /// Direct children by parent id (`parent_id` queries).
    children: std::collections::HashMap<Uuid, Vec<BaseItemEntity>>,
    /// Every descendant of the fixture item (the recursive `ancestor_ids` query).
    descendants: Vec<BaseItemEntity>,
    /// Stored `LockedFields` per item.
    locked: std::collections::HashMap<Uuid, Vec<MetadataField>>,
    /// `update_item_locked_fields` calls.
    locked_written: Mutex<Vec<(Uuid, Vec<i32>)>>,
    /// Other items the library resolves by id (a folder's parent).
    others: Vec<BaseItemEntity>,
}

/// The folder-refresh scans the library was asked to queue.
type RecordedScans = Arc<Mutex<Vec<(ScanTarget, MetadataRefreshOptions)>>>;

#[async_trait]
impl LibraryManager for OkLibrary {
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
        if let Some(other) = self.tree.others.iter().find(|row| row.id == id.to_string()) {
            return Ok(Some(other.clone()));
        }
        if let Some(root) = &self.tree.root {
            return Ok((id == self.item_id).then(|| root.clone()));
        }
        Ok((id == self.item_id).then(|| {
            let mut entity = base_item_entity(self.item_id);
            entity.is_folder = self.is_folder;
            entity.top_parent_id = self.top_parent_id.map(|id| id.to_string());
            entity
        }))
    }
    async fn queue_refresh_scan(
        &self,
        target: ScanTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        self.refresh_scans
            .lock()
            .unwrap()
            .push((target, options.clone()));
        Ok(())
    }
    async fn update_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        self.updated.lock().unwrap().extend(items.iter().cloned());
        Ok(())
    }
    async fn update_item_provider_ids(
        &self,
        item_id: Uuid,
        provider_ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        self.provider_ids
            .lock()
            .unwrap()
            .push((item_id, provider_ids.to_vec()));
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
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if query.recursive && query.ancestor_ids == [self.item_id] {
            return Ok(self.tree.descendants.clone());
        }
        // `RefreshArtist`'s query: the albums crediting the artist, which
        // the artist tests put among the library's other items.
        if !query.artist_ids.is_empty() {
            return Ok(self
                .tree
                .others
                .iter()
                .filter(|row| row.type_.ends_with("Audio.MusicAlbum"))
                .cloned()
                .collect());
        }
        let kinds: Vec<&str> = query
            .include_item_types
            .iter()
            .filter_map(|k| k.stored_type_name())
            .collect();
        Ok(self
            .tree
            .children
            .get(&query.parent_id)
            .into_iter()
            .flatten()
            .filter(|c| kinds.is_empty() || kinds.contains(&c.type_.as_str()))
            .cloned()
            .collect())
    }
    async fn get_locked_fields_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<MetadataField>>, ServiceError> {
        Ok(item_ids
            .iter()
            .filter_map(|id| self.tree.locked.get(id).map(|f| (*id, f.clone())))
            .collect())
    }
    async fn update_item_locked_fields(
        &self,
        item_id: Uuid,
        fields: &[i32],
    ) -> Result<(), ServiceError> {
        self.tree
            .locked_written
            .lock()
            .unwrap()
            .push((item_id, fields.to_vec()));
        Ok(())
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
    /// The options each queued refresh ran with.
    options: RecordedOptions,
}

/// The options of the provider refreshes the handler queued.
type RecordedOptions = Arc<Mutex<Vec<MetadataRefreshOptions>>>;

#[async_trait]
impl ProviderManager for RecordingProviders {
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        self.queued.lock().unwrap().push(item_id);
        self.options.lock().unwrap().push(options.clone());
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
/// metadata-editor handler can build its descriptor. Two cultures share a display
/// name (different casing) to exercise the handler's dedupe, and the list mixes
/// upper- and lower-case initials so an ordinal sort and a case-insensitive one
/// disagree on it. It is the same list `tests/localization.rs` feeds
/// `GET /Localization/Cultures`, so both endpoints can assert one expected order.
struct StubLocalization;

impl ferrofin_traits::localization::LocalizationManager for StubLocalization {
    fn get_cultures(&self) -> Vec<ferrofin_model::globalization::CultureDto> {
        ["Zulu", "English", "english", "German", "afar"]
            .into_iter()
            .enumerate()
            .map(
                |(i, display_name)| ferrofin_model::globalization::CultureDto {
                    name: format!("c{i}"),
                    display_name: display_name.to_owned(),
                    two_letter_iso_language_name: display_name[..2].to_ascii_lowercase(),
                    three_letter_iso_language_name: Some(display_name[..3].to_ascii_lowercase()),
                    three_letter_iso_language_names: vec![display_name[..3].to_ascii_lowercase()],
                },
            )
            .collect()
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
    fn get_localized_string(&self, phrase: &str) -> String {
        phrase.to_owned()
    }
    fn get_localized_string_for(&self, phrase: &str, _culture: &str) -> String {
        phrase.to_owned()
    }
    fn get_language_display_name(&self, _language: &str) -> Option<String> {
        None
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
            refresh_scans: Arc::default(),
            provider_ids: Arc::default(),
            updated: Arc::default(),
            tree: Arc::default(),
        }),
        queued,
    )
}

/// [`state`] with a caller-shaped [`OkLibrary`] (the folder-refresh tests).
fn state_with_library(library: Arc<OkLibrary>, queued: Arc<Mutex<Vec<Uuid>>>) -> AppState {
    state_recording_options(library, queued, RecordedOptions::default())
}

/// [`state_with_library`] recording the options of each provider refresh
/// into `options`.
fn state_recording_options(
    library: Arc<OkLibrary>,
    queued: Arc<Mutex<Vec<Uuid>>>,
    options: RecordedOptions,
) -> AppState {
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
        Arc::new(RecordingProviders { queued, options }),
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

/// Posts `body` to `/Items/{item_id}` against a capturing library and returns
/// the entity the handler wrote.
async fn update_and_capture(item_id: Uuid, body: String) -> BaseItemEntity {
    let updated: Arc<Mutex<Vec<BaseItemEntity>>> = Arc::default();
    let router = create_router(state_with_library(
        Arc::new(OkLibrary {
            item_id,
            is_folder: false,
            top_parent_id: None,
            refresh_scans: Arc::default(),
            provider_ids: Arc::default(),
            updated: updated.clone(),
            tree: Arc::default(),
        }),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let written = updated.lock().unwrap();
    written.first().expect("update written").clone()
}

/// `item.IsLocked = request.LockData ?? false`: an edit made with the
/// editor's "Lock this item" box unticked leaves the item unlocked (the old
/// auto-lock is gone — the scan now merges onto the stored row and honours
/// `LockedFields`, so the edit survives a rescan without it).
#[tokio::test]
async fn an_edit_without_lock_data_leaves_the_item_unlocked() {
    let item_id = Uuid::from_u128(0x59);
    for body in [
        r#""Name":"Renamed","LockData":false"#,
        r#""Name":"Renamed""#,
        r#""Name":"Renamed","LockData":null"#,
    ] {
        let written = update_and_capture(
            item_id,
            format!(r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video",{body}}}"#),
        )
        .await;
        assert_eq!(written.name.as_deref(), Some("Renamed"));
        assert!(!written.is_locked, "{body}: an edit never locks by itself");
    }
    let written = update_and_capture(
        item_id,
        format!(
            r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video","Name":"Renamed","LockData":true}}"#
        ),
    )
    .await;
    assert!(written.is_locked, "LockData=true locks");
}

/// Posts `body` for the fixture item of `library` and returns the library,
/// so a test can read everything the handler wrote.
async fn post_update(library: Arc<OkLibrary>, body: String) -> Arc<OkLibrary> {
    let item_id = library.item_id;
    let router = create_router(state_with_library(
        Arc::clone(&library),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    library
}

/// An [`OkLibrary`] around `tree`.
fn tree_library(item_id: Uuid, tree: Tree) -> Arc<OkLibrary> {
    Arc::new(OkLibrary {
        item_id,
        is_folder: false,
        top_parent_id: None,
        refresh_scans: Arc::default(),
        provider_ids: Arc::default(),
        updated: Arc::default(),
        tree: Arc::new(tree),
    })
}

/// `if (request.LockedFields is not null) item.LockedFields =
/// request.LockedFields;`: the set the editor sends is stored as sent (the
/// metadata editor sends the fields whose box is UNticked), and a body
/// without the key leaves the stored set alone.
#[tokio::test]
async fn locked_fields_are_stored_as_sent_and_absent_means_unchanged() {
    let item_id = Uuid::from_u128(0x59);
    let library = post_update(
        tree_library(item_id, Tree::default()),
        format!(
            r#"{{"Id":"{item_id}","Type":"Movie","Name":"Test Item","LockData":false,
                "LockedFields":["Overview","Name"]}}"#
        ),
    )
    .await;
    assert_eq!(
        *library.tree.locked_written.lock().unwrap(),
        // `MetadataField.Overview` = 6, `Name` = 5.
        vec![(item_id, vec![6, 5])]
    );
    let written = library.updated.lock().unwrap()[0].clone();
    assert!(!written.is_locked, "field locks do not lock the item");

    let library = post_update(
        tree_library(item_id, Tree::default()),
        format!(r#"{{"Id":"{item_id}","Type":"Movie","Name":"Test Item"}}"#),
    )
    .await;
    assert!(library.tree.locked_written.lock().unwrap().is_empty());

    // An empty list clears the set.
    let library = post_update(
        tree_library(item_id, Tree::default()),
        format!(r#"{{"Id":"{item_id}","Type":"Movie","Name":"Test Item","LockedFields":[]}}"#),
    )
    .await;
    assert_eq!(
        *library.tree.locked_written.lock().unwrap(),
        vec![(item_id, Vec::new())]
    );
}

/// A row of `kind` under `parent`.
fn child(id: u128, kind: &str, parent: Uuid, tags: Option<&str>) -> BaseItemEntity {
    BaseItemEntity {
        type_: format!("MediaBrowser.Controller.Entities.{kind}"),
        parent_id: Some(parent.to_string()),
        tags: tags.map(ToOwned::to_owned),
        official_rating: Some("Old rating".into()),
        custom_rating: Some("Old custom".into()),
        series_name: Some("Old series".into()),
        ..base_item_entity(Uuid::from_u128(id))
    }
}

/// `ItemUpdateController.UpdateItem`'s series walk (`:315-356`): the
/// series' name, rating, custom rating and tag edit reach its seasons and
/// their episodes, and each child's own `LockedFields` shields its
/// `OfficialRating` and `Tags`.
#[tokio::test]
async fn a_series_edit_cascades_to_seasons_and_episodes_honouring_their_locks() {
    let series_id = Uuid::from_u128(0x5E);
    let season = child(0x51, "TV.Season", series_id, Some("Old|Keep|Mine"));
    let season_id = Uuid::from_u128(0x51);
    let locked_rating = child(0xE1, "TV.Episode", season_id, Some("Old|Keep"));
    let open = child(0xE2, "TV.Episode", season_id, Some("Old|Keep"));
    let tree = Tree {
        root: Some(BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
            is_folder: true,
            tags: Some("Old|Keep".into()),
            ..base_item_entity(series_id)
        }),
        children: std::collections::HashMap::from([
            (series_id, vec![season]),
            (season_id, vec![locked_rating, open]),
        ]),
        locked: std::collections::HashMap::from([
            (season_id, vec![MetadataField::Tags]),
            (Uuid::from_u128(0xE1), vec![MetadataField::OfficialRating]),
        ]),
        ..Tree::default()
    };
    let library = post_update(
        tree_library(series_id, tree),
        format!(
            r#"{{"Id":"{series_id}","Type":"Series","Name":"New Series",
                "OfficialRating":"TV-MA","CustomRating":"New custom",
                "Tags":["Keep","New"]}}"#
        ),
    )
    .await;
    let written = library.updated.lock().unwrap().clone();
    let by_id = |id: u128| {
        written
            .iter()
            .rev()
            .find(|e| e.id == Uuid::from_u128(id).to_string())
            .cloned()
            .expect("child written")
    };
    let season = by_id(0x51);
    assert_eq!(season.series_name.as_deref(), Some("New Series"));
    assert_eq!(season.official_rating.as_deref(), Some("TV-MA"));
    assert_eq!(season.custom_rating.as_deref(), Some("New custom"));
    assert_eq!(season.tags.as_deref(), Some("Old|Keep|Mine"), "Tags locked");
    let locked = by_id(0xE1);
    assert_eq!(
        locked.official_rating.as_deref(),
        Some("Old rating"),
        "rating locked"
    );
    assert_eq!(
        locked.custom_rating.as_deref(),
        Some("New custom"),
        "never lock-checked"
    );
    assert_eq!(
        locked.tags.as_deref(),
        Some("Keep|New"),
        "Old removed, New added"
    );
    assert_eq!(locked.series_name.as_deref(), Some("New Series"));
    let open = by_id(0xE2);
    assert_eq!(open.official_rating.as_deref(), Some("TV-MA"));
    assert_eq!(open.tags.as_deref(), Some("Keep|New"));
}

/// The season and album walks (`:357-381`): episodes / tracks take the
/// rating and tag edit, not the series name.
#[tokio::test]
async fn season_and_album_edits_cascade_to_their_children() {
    for (kind, child_kind) in [
        ("TV.Season", "TV.Episode"),
        ("Audio.MusicAlbum", "Audio.Audio"),
    ] {
        let parent = Uuid::from_u128(0x70);
        let kid = child(0x71, child_kind, parent, None);
        let tree = Tree {
            root: Some(BaseItemEntity {
                type_: format!("MediaBrowser.Controller.Entities.{kind}"),
                is_folder: true,
                ..base_item_entity(parent)
            }),
            children: std::collections::HashMap::from([(parent, vec![kid])]),
            ..Tree::default()
        };
        let library = post_update(
            tree_library(parent, tree),
            format!(r#"{{"Id":"{parent}","Name":"P","OfficialRating":"  ","Tags":["T"]}}"#),
        )
        .await;
        let written = library.updated.lock().unwrap().clone();
        let kid = written
            .iter()
            .find(|e| e.id == Uuid::from_u128(0x71).to_string())
            .expect("child written");
        assert_eq!(kid.official_rating, None, "{kind}: a blank rating clears");
        assert_eq!(kid.tags.as_deref(), Some("T"), "{kind}");
        assert_eq!(kid.series_name.as_deref(), Some("Old series"), "{kind}");
    }
}

/// `if (isLockedChanged && item.IsFolder)`: a change of `LockData` on a
/// folder reaches every descendant; an unchanged one touches none.
#[tokio::test]
async fn a_lock_change_on_a_folder_cascades_to_every_descendant() {
    let folder = Uuid::from_u128(0x80);
    let descendants = vec![
        child(0x81, "TV.Season", folder, None),
        child(0x82, "TV.Episode", Uuid::from_u128(0x81), None),
    ];
    let tree = || Tree {
        root: Some(BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Folder".into(),
            is_folder: true,
            ..base_item_entity(folder)
        }),
        descendants: descendants.clone(),
        ..Tree::default()
    };
    let library = post_update(
        tree_library(folder, tree()),
        format!(r#"{{"Id":"{folder}","Name":"F","LockData":true}}"#),
    )
    .await;
    let written = library.updated.lock().unwrap().clone();
    assert_eq!(written.len(), 3, "the folder, then both descendants");
    assert!(written.iter().all(|e| e.is_locked));

    // Unchanged (stored unlocked, sent unlocked): no descendant is written.
    let library = post_update(
        tree_library(folder, tree()),
        format!(r#"{{"Id":"{folder}","Name":"F","LockData":false}}"#),
    )
    .await;
    assert_eq!(library.updated.lock().unwrap().len(), 1);
}

/// A save that changes nothing honors the checkbox: LockData=false stays
/// unlocked (this is how an item is un-locked from the editor).
#[tokio::test]
async fn unchanged_save_respects_unlock() {
    let item_id = Uuid::from_u128(0x59);
    // Round-trip the fixture's stored values verbatim ("Test Item", no other
    // editable fields set) with the lock checkbox unticked.
    let written = update_and_capture(
        item_id,
        format!(
            r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video","Name":"Test Item","LockData":false}}"#
        ),
    )
    .await;
    assert_eq!(written.name.as_deref(), Some("Test Item"));
    assert!(
        !written.is_locked,
        "an unchanged save with LockData=false must not re-lock"
    );
}

/// The editor's external ids are persisted, empty values are dropped, and the
/// write REPLACES the stored set — C# `ItemUpdateController.UpdateItem` strips
/// empty pairs and then assigns (`item.ProviderIds = request.ProviderIds`,
/// v10.11.8 lines 402-410), so a key the client omitted is gone afterwards.
/// Before this, the handler parsed the DTO and silently dropped `ProviderIds`,
/// which lost the id every "Identify" and every hand-typed IMDb/TVDB id.
#[tokio::test]
async fn update_item_replaces_the_external_ids() {
    let item_id = Uuid::from_u128(0x59);
    let recorded: RecordedProviderIds = Arc::default();
    let router = create_router(state_with_library(
        Arc::new(OkLibrary {
            item_id,
            is_folder: false,
            top_parent_id: None,
            refresh_scans: Arc::default(),
            provider_ids: recorded.clone(),
            updated: Arc::default(),
            tree: Arc::default(),
        }),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video","Name":"Test Item",
                        "ProviderIds":{{"Imdb":"tt0111161","Tmdb":"278","Tvdb":""}}}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let written = recorded.lock().unwrap();
    let (id, ids) = written.first().expect("provider ids written").clone();
    assert_eq!(id, item_id);
    assert_eq!(
        ids,
        vec![
            ("Imdb".to_owned(), "tt0111161".to_owned()),
            ("Tmdb".to_owned(), "278".to_owned()),
        ],
        "empty values are stripped; the rest are written as the whole new set"
    );
}

/// A body with no `ProviderIds` key leaves the stored ids alone rather than
/// clearing them: the vendored contract types the field `nullable: true`, so an
/// absent key is a legal request that says nothing about the ids.
#[tokio::test]
async fn update_item_without_provider_ids_leaves_them_alone() {
    let item_id = Uuid::from_u128(0x59);
    let recorded: RecordedProviderIds = Arc::default();
    let router = create_router(state_with_library(
        Arc::new(OkLibrary {
            item_id,
            is_folder: false,
            top_parent_id: None,
            refresh_scans: Arc::default(),
            provider_ids: recorded.clone(),
            updated: Arc::default(),
            tree: Arc::default(),
        }),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video","Name":"Renamed"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        recorded.lock().unwrap().is_empty(),
        "an absent ProviderIds key must not write (and so must not clear) the id set"
    );
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

/// A folder row of `kind` at `path` for the folder-refresh tests.
fn folder_row(id: Uuid, kind: &str, path: Option<&str>, parent: Option<Uuid>) -> BaseItemEntity {
    let mut row = base_item_entity(id);
    kind.clone_into(&mut row.type_);
    row.is_folder = true;
    row.path = path.map(str::to_owned);
    row.parent_id = parent.map(|p| p.to_string());
    row
}

/// Posts `/Items/{id}/Refresh{query}` against a library resolving `root` (and
/// `others`), returning the scans it queued and the provider refreshes.
async fn refresh_folder(
    root: BaseItemEntity,
    others: Vec<BaseItemEntity>,
    query: &str,
) -> (Vec<(ScanTarget, MetadataRefreshOptions)>, Vec<Uuid>) {
    let id = Uuid::parse_str(&root.id).expect("id");
    let scans: RecordedScans = Arc::default();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state_with_library(
        Arc::new(OkLibrary {
            item_id: id,
            is_folder: true,
            top_parent_id: None,
            refresh_scans: scans.clone(),
            provider_ids: Arc::default(),
            updated: Arc::default(),
            tree: Arc::new(Tree {
                root: Some(root),
                others,
                ..Tree::default()
            }),
        }),
        queued.clone(),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{id}/Refresh{query}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let scans = scans.lock().unwrap().clone();
    let queued = queued.lock().unwrap().clone();
    (scans, queued)
}

/// What jellyfin-web's refresh dialog sends for each choice
/// (`refreshdialog.js:76-90`), with "Replace existing images" ticked for the
/// two full refreshes — its PascalCase keys as the server binary's query-key
/// fold hands them to the router (`apps/ferrofin-server` lower-cases each
/// key's first letter before routing).
const SCAN_FOR_NEW: &str = "?recursive=true&imageRefreshMode=Default&metadataRefreshMode=Default&replaceAllImages=false&regenerateTrickplay=false&replaceAllMetadata=false";
const SEARCH_MISSING: &str = "?recursive=true&imageRefreshMode=FullRefresh&metadataRefreshMode=FullRefresh&replaceAllImages=true&regenerateTrickplay=true&replaceAllMetadata=false";
const REPLACE_ALL: &str = "?recursive=true&imageRefreshMode=FullRefresh&metadataRefreshMode=FullRefresh&replaceAllImages=true&regenerateTrickplay=false&replaceAllMetadata=true";

/// `POST /Items/{itemId}/Refresh` on a library's CollectionFolder scans that
/// library — never every library — with the request's options, for each of
/// the dashboard's three choices (`ProviderManager.RefreshCollectionFolderChildren`
/// validates the library's folders with the same options).
#[tokio::test]
async fn refresh_library_folder_scans_that_library_with_the_request_options() {
    let folder_id = Uuid::from_u128(0x11B);
    let library = || {
        folder_row(
            folder_id,
            "MediaBrowser.Controller.Entities.CollectionFolder",
            Some("/config/root/default/Movies"),
            None,
        )
    };
    let (scans, queued) = refresh_folder(library(), Vec::new(), SCAN_FOR_NEW).await;
    // "Scan for new and updated files" is the scan's own default refresh.
    assert_eq!(
        scans,
        vec![(
            ScanTarget::Library(folder_id),
            MetadataRefreshOptions::default()
        )]
    );
    assert!(
        queued.is_empty(),
        "a folder refresh drives the scan, not the provider queue"
    );

    let (scans, _) = refresh_folder(library(), Vec::new(), SEARCH_MISSING).await;
    let expected = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_images: true,
        force_save: true,
        regenerate_trickplay: true,
        ..MetadataRefreshOptions::default()
    };
    assert_eq!(scans, vec![(ScanTarget::Library(folder_id), expected)]);

    let (scans, _) = refresh_folder(library(), Vec::new(), REPLACE_ALL).await;
    let expected = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        remove_old_metadata: true,
        force_save: true,
        ..MetadataRefreshOptions::default()
    };
    assert_eq!(scans, vec![(ScanTarget::Library(folder_id), expected)]);
}

/// Refreshing a folder nested inside a library (a series, season, album)
/// scans THAT folder's subtree — `Folder.ValidateChildren` — not its whole
/// library. With neither mode given, both are `None`
/// (`ItemRefreshController.cs:64-65`) and nothing is forced.
#[tokio::test]
async fn refresh_nested_folder_scans_only_its_subtree() {
    let series_id = Uuid::from_u128(0x5E1);
    let series = folder_row(
        series_id,
        "MediaBrowser.Controller.Entities.TV.Series",
        Some("/media/tv/Firefly"),
        Some(Uuid::from_u128(0x11B2)),
    );
    let (scans, queued) = refresh_folder(series, Vec::new(), "").await;
    assert_eq!(
        scans,
        vec![(
            ScanTarget::Paths(vec!["/media/tv/Firefly".to_owned()]),
            MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::None,
                image_refresh_mode: MetadataRefreshMode::None,
                ..MetadataRefreshOptions::default()
            }
        )]
    );
    assert!(queued.is_empty());
}

/// A virtual season (episodes straight in the series folder) has no folder
/// of its own, so upstream validates no children for it (not
/// `IsFileProtocol`, `Folder.cs:430-436`): it refreshes itself only — never
/// its whole series, which a "Replace all" would otherwise hit season by
/// season.
#[tokio::test]
async fn refresh_virtual_season_refreshes_only_itself() {
    let series_id = Uuid::from_u128(0x5E2);
    let season_id = Uuid::from_u128(0x5E3);
    let series = folder_row(
        series_id,
        "MediaBrowser.Controller.Entities.TV.Series",
        Some("/media/tv/Flat Show"),
        None,
    );
    let season = folder_row(
        season_id,
        "MediaBrowser.Controller.Entities.TV.Season",
        None,
        Some(series_id),
    );
    let (scans, queued) = refresh_folder(season, vec![series], REPLACE_ALL).await;
    assert!(scans.is_empty(), "no scan: {scans:?}");
    assert_eq!(queued, vec![season_id], "the season refreshes itself");
}

/// `ProviderManager.RefreshArtist` for an artist known only by name (a
/// compilation's album artist, pathed in the metadata folder): the artist
/// folders its credited albums sit under are scanned with the request's
/// options, an album with no artist folder above it is not (its artist is
/// the by-name one, whose validation is a no-op upstream), and the artist
/// refreshes itself — in the same scan (`path: None`), whose music pass runs
/// its MusicBrainz/TheAudioDB providers, never beside it through the
/// provider queue.
#[tokio::test]
async fn refresh_by_name_artist_scans_its_albums_artist_folders_and_refreshes_itself() {
    let artist_id = Uuid::from_u128(0xA1);
    let (folder_a, folder_b) = (Uuid::from_u128(0xFA), Uuid::from_u128(0xFB));
    let artist_kind = "MediaBrowser.Controller.Entities.Audio.MusicArtist";
    let album_kind = "MediaBrowser.Controller.Entities.Audio.MusicAlbum";
    let by_name = folder_row(
        artist_id,
        artist_kind,
        Some("/config/metadata/artists/Various"),
        None,
    );
    let mut others = Vec::new();
    for (id, path) in [(folder_a, "/music/Artist A"), (folder_b, "/music/Artist B")] {
        let mut row = folder_row(id, artist_kind, Some(path), Some(Uuid::from_u128(0x11B)));
        row.top_parent_id = Some(Uuid::from_u128(0x11B).to_string());
        others.push(row);
    }
    for (id, parent) in [
        (0xAB1, Some(folder_a)),
        (0xAB2, Some(folder_a)),
        (0xAB3, Some(folder_b)),
        (0xAB4, None),
    ] {
        others.push(folder_row(
            Uuid::from_u128(id),
            album_kind,
            Some("/music/x"),
            parent,
        ));
    }
    let (scans, queued) = refresh_folder(by_name, others, SEARCH_MISSING).await;
    assert_eq!(scans.len(), 1);
    assert_eq!(
        scans[0].0,
        ScanTarget::Artist {
            id: artist_id,
            path: None,
            folders: vec!["/music/Artist A".to_owned(), "/music/Artist B".to_owned()],
        },
        "the artist folders' children are validated, not those artists"
    );
    assert_eq!(
        scans[0].1.metadata_refresh_mode,
        MetadataRefreshMode::FullRefresh
    );
    assert!(
        queued.is_empty(),
        "the scan refreshes the by-name artist itself: {queued:?}"
    );

    // With no credited album under an artist folder there is nothing to
    // validate: the scan target is the artist alone.
    let by_name = folder_row(
        artist_id,
        artist_kind,
        Some("/config/metadata/artists/Various"),
        None,
    );
    let (scans, queued) = refresh_folder(by_name, Vec::new(), SEARCH_MISSING).await;
    assert_eq!(
        scans.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        vec![ScanTarget::Artist {
            id: artist_id,
            path: None,
            folders: Vec::new(),
        }]
    );
    assert!(queued.is_empty(), "{queued:?}");
}

/// A folder-backed artist refreshes itself in the scan, and only the artist
/// folders its credited albums sit under are validated — its own when an
/// album of its sits there, another artist's for a collaboration filed
/// under that artist — never its own folder by default.
#[tokio::test]
async fn refresh_folder_backed_artist_validates_only_its_albums_folders() {
    let artist_id = Uuid::from_u128(0xA2);
    let other_id = Uuid::from_u128(0xA3);
    let artist_kind = "MediaBrowser.Controller.Entities.Audio.MusicArtist";
    let library = Uuid::from_u128(0x11B);
    let folder = |id, path| {
        let mut row = folder_row(id, artist_kind, Some(path), Some(library));
        row.top_parent_id = Some(library.to_string());
        row
    };
    let artist = folder(artist_id, "/music/Artist A");
    let other = folder(other_id, "/music/Artist B");
    // Its only credited album is a collaboration filed under Artist B.
    let album = folder_row(
        Uuid::from_u128(0xAB1),
        "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
        Some("/music/Artist B/Duets"),
        Some(other_id),
    );
    let (scans, queued) =
        refresh_folder(artist.clone(), vec![artist, other, album], REPLACE_ALL).await;
    assert_eq!(scans.len(), 1);
    assert_eq!(
        scans[0].0,
        ScanTarget::Artist {
            id: artist_id,
            path: Some("/music/Artist A".to_owned()),
            folders: vec!["/music/Artist B".to_owned()],
        }
    );
    assert!(scans[0].1.replace_all_metadata);
    assert!(queued.is_empty(), "the scan refreshes the artist itself");
}

/// The server root refreshes every library.
#[tokio::test]
async fn refresh_root_folder_scans_every_library() {
    let root = folder_row(
        Uuid::from_u128(0xA66),
        "MediaBrowser.Controller.Entities.AggregateFolder",
        Some("/config/root"),
        None,
    );
    let (scans, _) = refresh_folder(root, Vec::new(), SCAN_FOR_NEW).await;
    assert_eq!(
        scans,
        vec![(ScanTarget::All, MetadataRefreshOptions::default())]
    );
}

/// A box set's members and a playlist's entries are linked, not physical:
/// upstream validates no children for them (`BoxSet.GetNonCachedChildren`,
/// `Playlist.ValidateChildrenInternal`), so their refresh is the item's own —
/// the provider queue — and never a scan (it used to fall back to a scan of
/// every library).
#[tokio::test]
async fn refresh_box_set_and_playlist_refresh_themselves() {
    for kind in [
        "MediaBrowser.Controller.Entities.Movies.BoxSet",
        "MediaBrowser.Controller.Playlists.Playlist",
    ] {
        let id = Uuid::from_u128(0xB0);
        let (scans, queued) = refresh_folder(
            folder_row(id, kind, Some("/config/data/collections/Set"), None),
            Vec::new(),
            REPLACE_ALL,
        )
        .await;
        assert!(scans.is_empty(), "{kind} is not scanned");
        assert_eq!(queued, vec![id], "{kind} refreshes itself");
    }
}

/// Posts `/Items/{id}/Refresh{query}` for the item row `root`, returning the
/// scans it queued and the provider refreshes' item ids and options.
async fn refresh_row(
    root: BaseItemEntity,
    query: &str,
) -> (
    Vec<(ScanTarget, MetadataRefreshOptions)>,
    Vec<Uuid>,
    Vec<MetadataRefreshOptions>,
) {
    let id = Uuid::parse_str(&root.id).expect("id");
    let scans: RecordedScans = Arc::default();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let options = RecordedOptions::default();
    let router = create_router(state_recording_options(
        Arc::new(OkLibrary {
            item_id: id,
            is_folder: root.is_folder,
            top_parent_id: None,
            refresh_scans: scans.clone(),
            provider_ids: Arc::default(),
            updated: Arc::default(),
            tree: Arc::new(Tree {
                root: Some(root),
                ..Tree::default()
            }),
        }),
        queued.clone(),
        options.clone(),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{id}/Refresh{query}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let scans = scans.lock().unwrap().clone();
    let queued = queued.lock().unwrap().clone();
    let options = options.lock().unwrap().clone();
    (scans, queued, options)
}

/// The options `ItemRefreshController` builds for a dashboard choice.
fn controller_options(query: &str) -> MetadataRefreshOptions {
    use ferrofin_traits::providers::MetadataRefreshMode;
    match query {
        SCAN_FOR_NEW => MetadataRefreshOptions::for_item_refresh(
            MetadataRefreshMode::Default,
            MetadataRefreshMode::Default,
            false,
            false,
            false,
        ),
        SEARCH_MISSING => MetadataRefreshOptions::for_item_refresh(
            MetadataRefreshMode::FullRefresh,
            MetadataRefreshMode::FullRefresh,
            false,
            true,
            true,
        ),
        _ => MetadataRefreshOptions::for_item_refresh(
            MetadataRefreshMode::FullRefresh,
            MetadataRefreshMode::FullRefresh,
            true,
            true,
            false,
        ),
    }
}

/// Phase 5b: `POST /Items/{itemId}/Refresh` on a file item — a movie, an
/// episode, a track, a book, a photo — is the library scan of its own path
/// with the request's options (`ForceSave`/`RemoveOldMetadata` by the
/// controller's rule), so it gets the scan's decision, merge, locks, probe,
/// NFO and every provider; the provider queue is not used, and nothing else
/// of its library is walked.
#[tokio::test]
async fn refresh_a_file_item_scans_its_own_path_with_the_request_options() {
    for kind in [
        "MediaBrowser.Controller.Entities.Movies.Movie",
        "MediaBrowser.Controller.Entities.TV.Episode",
        "MediaBrowser.Controller.Entities.Audio.Audio",
        "MediaBrowser.Controller.Entities.Book",
        "MediaBrowser.Controller.Entities.Photo",
    ] {
        for query in [SCAN_FOR_NEW, SEARCH_MISSING, REPLACE_ALL] {
            let id = Uuid::from_u128(0xF11E);
            let mut row = base_item_entity(id);
            kind.clone_into(&mut row.type_);
            row.is_folder = false;
            row.path = Some("/media/lib/Some Item/file.ext".to_owned());
            row.top_parent_id = Some(Uuid::from_u128(0x11B).to_string());
            let (scans, queued, _) = refresh_row(row, query).await;
            assert_eq!(
                scans,
                vec![(
                    ScanTarget::Items(vec!["/media/lib/Some Item/file.ext".to_owned()]),
                    controller_options(query)
                )],
                "{kind} {query}"
            );
            assert!(queued.is_empty(), "{kind}: the provider queue is not used");
        }
    }
}

/// An item with no file of its own in a library — a person, a channel item
/// streamed from a URL, a row outside every library — refreshes through the
/// provider queue, now with the controller's options (`ForceSave`,
/// `RemoveOldMetadata`), where it used to drop both.
#[tokio::test]
async fn refresh_an_item_with_no_file_refreshes_through_the_provider_queue() {
    let person = {
        let mut row = base_item_entity(Uuid::from_u128(0xFE));
        "MediaBrowser.Controller.Entities.Person".clone_into(&mut row.type_);
        row.is_folder = false;
        row.path = Some("/config/metadata/People/A/Actor".to_owned());
        row.top_parent_id = None;
        row
    };
    let streamed = {
        let mut row = base_item_entity(Uuid::from_u128(0xFF));
        row.is_folder = false;
        row.path = Some("http://tuner.local/stream/7".to_owned());
        row.top_parent_id = Some(Uuid::from_u128(0x11B).to_string());
        row
    };
    for row in [person, streamed] {
        let id = Uuid::parse_str(&row.id).expect("id");
        let (scans, queued, options) = refresh_row(row, REPLACE_ALL).await;
        assert!(scans.is_empty());
        assert_eq!(queued, vec![id]);
        assert_eq!(options, vec![controller_options(REPLACE_ALL)]);
        assert!(options[0].force_save && options[0].remove_old_metadata);
    }
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

    // Cultures are deduped case-insensitively and ordered case-insensitively, the
    // same way `GET /Localization/Cultures` orders them (C#'s comparer-less
    // `OrderBy(c => c.DisplayName)` is linguistic, not ordinal, so "afar" precedes
    // "Zulu"; an ordinal sort would yield ["English", "German", "Zulu", "afar"]).
    // `tests/localization.rs::EXPECTED_CULTURE_ORDER` asserts this same sequence
    // for the same stub list — the two lists a client cross-references must agree.
    let names: Vec<&str> = info
        .cultures
        .iter()
        .map(|c| c.display_name.as_str())
        .collect();
    assert_eq!(names, ["afar", "English", "German", "Zulu"]);
    // The dedupe keeps the first entry in source order: "English" (c1), not
    // "english" (c2).
    assert_eq!(info.cultures[1].name, "c1");
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

/// `GetRecursiveChildren()` includes the folder's linked children
/// (`AddChildrenToList(includeLinkedChildren: true)`, `Folder.cs:1654-1682`):
/// locking a box set locks its member movies, which are not its physical
/// descendants.
#[tokio::test]
async fn a_lock_change_on_a_box_set_reaches_its_linked_members() {
    let boxset = Uuid::from_u128(0x90);
    let member = child(0x91, "Movies.Movie", Uuid::from_u128(0x99), None);
    let tree = Tree {
        root: Some(BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Movies.BoxSet".into(),
            is_folder: true,
            ..base_item_entity(boxset)
        }),
        // Only the non-physical `parent_id` browse (which merges
        // `LinkedChildren`) finds the member; the recursive ancestor query
        // finds nothing.
        children: std::collections::HashMap::from([(boxset, vec![member])]),
        ..Tree::default()
    };
    let library = post_update(
        tree_library(boxset, tree),
        format!(r#"{{"Id":"{boxset}","Name":"B","LockData":true}}"#),
    )
    .await;
    let written = library.updated.lock().unwrap().clone();
    let member = written
        .iter()
        .find(|e| e.id == Uuid::from_u128(0x91).to_string())
        .expect("member written");
    assert!(member.is_locked);
}
