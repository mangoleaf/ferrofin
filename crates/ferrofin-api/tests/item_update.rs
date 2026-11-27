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
    FakeSystem, FakeUserData, FakeUserViews, FixedAuth, fake_user_entity,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::{MetadataEditorInfo, NameValuePair};
use ferrofin_model::entities::MetadataField;
use ferrofin_model::providers::ExternalIdInfo;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
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
        password: Some("hashed".to_owned()),
        ..fake_user_entity(id, username)
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
        // Stored, so an edit that omits `OriginalLanguage` visibly clears it.
        original_language: Some("ja".to_owned()),
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

/// An `AuthService`/`AuthorizationContext` that authenticates as [`USER_ID`].
/// Every route this file covers — `POST /Items/{itemId}`, `ContentType`,
/// `MetadataEditor`, `Refresh`, `ExternalIdInfos` — is `RequiresElevation` upstream,
/// so this stub authenticates as an API key — which satisfies the policy
/// without a user/policy lookup, exactly as C# does. The gate itself is pinned
/// end to end in `apps/ferrofin-server/tests/elevation.rs`.
fn ok_auth() -> FixedAuth {
    FixedAuth(AuthorizationInfo {
        user: Some(Arc::new(user_entity(USER_ID, "alice"))),
        is_api_key: true,
        is_authenticated: true,
        ..AuthorizationInfo::default()
    })
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
    async fn ensure_sign_in_allowed(
        &self,
        _user: &UserEntity,
        _remote_endpoint: &str,
    ) -> Result<(), ServiceError> {
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
/// fields shape the resolved entity; the refresh tests assert against
/// `refreshes`.
struct OkLibrary {
    item_id: Uuid,
    is_folder: bool,
    top_parent_id: Option<Uuid>,
    refreshes: RecordedRefreshes,
    /// Entities passed to `update_items`, for asserting what the edit wrote.
    updated: Arc<Mutex<Vec<BaseItemEntity>>>,
    /// External-id sets passed to `update_item_provider_ids` — a second write,
    /// because `BaseItemProviders` is its own table.
    provider_ids: RecordedProviderIds,
    /// The item's children and locked fields, for the cascade tests.
    tree: Arc<Tree>,
    /// The item's metadata lock as the providers hand it out: `update_items`
    /// panics unless it is held, so an edit that writes without it fails.
    lock_probe: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl OkLibrary {
    /// Panics when a `lock_probe` is set and not held: the edit wrote the item
    /// outside its metadata lock.
    fn assert_metadata_locked(&self) {
        if let Some(lock) = &self.lock_probe {
            assert!(
                lock.try_lock().is_err(),
                "the edit wrote the item without its metadata lock"
            );
        }
    }
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
    /// What `get_inherited_content_type` answers: `None` is a mixed library.
    inherited_content_type: Option<CollectionType>,
    /// What `get_configured_content_type` answers: the item's override.
    configured_content_type: Option<CollectionType>,
    /// `update_people` calls: the item and the credits written.
    people_written: Mutex<Vec<(String, Vec<PeopleEntity>)>>,
}

/// The refreshes the library was asked to queue: the item and the options.
type RecordedRefreshes = Arc<Mutex<Vec<(Uuid, MetadataRefreshOptions)>>>;

#[async_trait]
impl LibraryManager for OkLibrary {
    async fn update_people(
        &self,
        item: &BaseItemEntity,
        people: &[PeopleEntity],
    ) -> Result<(), ServiceError> {
        self.assert_metadata_locked();
        self.tree
            .people_written
            .lock()
            .unwrap()
            .push((item.id.clone(), people.to_vec()));
        Ok(())
    }

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
    async fn queue_item_refresh(
        &self,
        item: &BaseItemEntity,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        self.refreshes
            .lock()
            .unwrap()
            .push((Uuid::parse_str(&item.id).expect("id"), options.clone()));
        Ok(())
    }
    async fn update_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        self.assert_metadata_locked();
        self.updated.lock().unwrap().extend(items.iter().cloned());
        Ok(())
    }
    async fn update_item_provider_ids(
        &self,
        item_id: Uuid,
        provider_ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        self.assert_metadata_locked();
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
        if query.recursive
            && query.ancestor_ids == [self.item_id]
            && !self.tree.descendants.is_empty()
        {
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
        // The cascade queries a season's episodes by ancestor_ids.
        if !query.ancestor_ids.is_empty() {
            return Ok(query
                .ancestor_ids
                .iter()
                .filter_map(|id| self.tree.children.get(id))
                .flatten()
                .filter(|c| kinds.is_empty() || kinds.contains(&c.type_.as_str()))
                .cloned()
                .collect());
        }
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
        self.assert_metadata_locked();
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
    async fn get_inherited_content_type(
        &self,
        _item: &BaseItemEntity,
    ) -> Result<Option<CollectionType>, ServiceError> {
        Ok(self.tree.inherited_content_type)
    }
    async fn get_configured_content_type(
        &self,
        _item: &BaseItemEntity,
    ) -> Result<Option<CollectionType>, ServiceError> {
        Ok(self.tree.configured_content_type)
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
}

/// A [`ProviderManager`] that records the last queued refresh (so the refresh
/// handler is observable) and advertises a single external-id descriptor (so the
/// external-id / metadata-editor routes return data).
struct RecordingProviders {
    queued: Arc<Mutex<Vec<Uuid>>>,
    /// The options each queued refresh ran with.
    options: RecordedOptions,
    /// What `lock_item_metadata` locks, for every item.
    metadata_lock: MetadataLock,
}

/// The one lock the fake providers hand out for any item, and the items it
/// was asked for: a test holds `lock` to stand in for a running refresh.
#[derive(Clone, Default)]
struct MetadataLock {
    lock: Arc<tokio::sync::Mutex<()>>,
    asked: Arc<Mutex<Vec<Uuid>>>,
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
    async fn lock_item_metadata(&self, item_id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        self.metadata_lock.asked.lock().unwrap().push(item_id);
        Arc::clone(&self.metadata_lock.lock).lock_owned().await
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
        unreachable!("the editor's saves, the cascades' included, end in update_to_repository")
    }
    /// `UpdateToRepositoryAsync(MetadataEdit)`, which measures the images
    /// (`UpdateImagesAsync`) where `save_metadata` only runs the savers.
    async fn update_to_repository(
        &self,
        item_id: Uuid,
        update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        assert!(!item_id.is_nil());
        assert_eq!(update_type, ItemUpdateType::MetadataEdit);
        Ok(())
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
        self.get_cultures().into_iter().find(|culture| {
            language.eq_ignore_ascii_case(&culture.display_name)
                || language.eq_ignore_ascii_case(&culture.two_letter_iso_language_name)
                || culture
                    .three_letter_iso_language_names
                    .iter()
                    .any(|code| language.eq_ignore_ascii_case(code))
        })
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
            refreshes: Arc::default(),
            provider_ids: Arc::default(),
            updated: Arc::default(),
            tree: Arc::default(),
            lock_probe: None,
        }),
        queued,
    )
}

/// [`state`] with a caller-shaped [`OkLibrary`] (the folder-refresh tests).
fn state_with_library(library: Arc<OkLibrary>, queued: Arc<Mutex<Vec<Uuid>>>) -> AppState {
    state_recording_options(library, queued, RecordedOptions::default())
}

/// [`state_with_library`] over `config` (the content-type tests).
fn state_with_library_and_config(
    library: Arc<OkLibrary>,
    config: Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager>,
) -> AppState {
    build_state(
        library,
        Arc::default(),
        RecordedOptions::default(),
        config,
        MetadataLock::default(),
    )
}

/// [`state_with_library`] recording the options of each provider refresh
/// into `options`.
fn state_recording_options(
    library: Arc<OkLibrary>,
    queued: Arc<Mutex<Vec<Uuid>>>,
    options: RecordedOptions,
) -> AppState {
    build_state(
        library,
        queued,
        options,
        Arc::new(FakeConfig),
        MetadataLock::default(),
    )
}

/// The item-update [`AppState`] over `config`, whose providers lock
/// `metadata_lock` for every item.
fn build_state(
    library: Arc<OkLibrary>,
    queued: Arc<Mutex<Vec<Uuid>>>,
    options: RecordedOptions,
    config: Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager>,
    metadata_lock: MetadataLock,
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
        config,
        Arc::new(RecordingProviders {
            queued,
            options,
            metadata_lock,
        }),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(ok_auth()),
        Arc::new(ok_auth()),
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
            refreshes: Arc::default(),
            provider_ids: Arc::default(),
            updated: updated.clone(),
            tree: Arc::default(),
            lock_probe: None,
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

/// An edit runs under the item's metadata lock: while a refresh of the item
/// holds it the edit waits, and it keeps it while it writes (the library's
/// `lock_probe`), so it lands over the refresh's save rather than under it
/// (gl-1852).
#[tokio::test]
async fn an_edit_waits_for_the_items_metadata_lock() {
    let item_id = Uuid::from_u128(0x59);
    let updated: Arc<Mutex<Vec<BaseItemEntity>>> = Arc::default();
    let provider_ids = RecordedProviderIds::default();
    let tree = Arc::new(Tree::default());
    let lock = MetadataLock::default();
    let refresh = Arc::clone(&lock.lock).lock_owned().await;
    let router = create_router(build_state(
        Arc::new(OkLibrary {
            item_id,
            is_folder: false,
            top_parent_id: None,
            refreshes: Arc::default(),
            provider_ids: provider_ids.clone(),
            updated: updated.clone(),
            tree: tree.clone(),
            lock_probe: Some(Arc::clone(&lock.lock)),
        }),
        Arc::default(),
        RecordedOptions::default(),
        Arc::new(FakeConfig),
        lock.clone(),
    ));
    let edit = tokio::spawn(
        router.oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"Name":"Saga","Overview":"Edited overview","People":[],"ProviderIds":{"Tmdb":"7"},"LockedFields":["Overview"]}"#,
                ))
                .unwrap(),
        ),
    );
    // Enough turns for an edit that ignored the lock to have written.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        updated.lock().unwrap().is_empty(),
        "the edit wrote while a refresh held the item"
    );
    drop(refresh);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), edit)
        .await
        .expect("the edit finished once the lock was free")
        .expect("join")
        .expect("response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        *lock.asked.lock().unwrap(),
        [item_id],
        "the edit locked its own item"
    );
    let written = updated.lock().unwrap();
    assert_eq!(
        written.first().and_then(|row| row.overview.as_deref()),
        Some("Edited overview")
    );
    // Every write of the item's own tables ran, each under the lock.
    assert_eq!(tree.people_written.lock().unwrap().len(), 1);
    assert_eq!(provider_ids.lock().unwrap().len(), 1);
    assert_eq!(tree.locked_written.lock().unwrap().len(), 1);
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

/// `item.OriginalLanguage = string.IsNullOrWhiteSpace(request.OriginalLanguage)
/// ? null : request.OriginalLanguage` (v12.2 `ItemUpdateController.cs:245`)
/// through the `BaseItem.OriginalLanguage` setter: a known language is stored
/// as its two-letter code, an unknown one untrimmed as sent, whitespace as
/// null, and an absent member clears the stored `"ja"`.
#[tokio::test]
async fn an_edit_stores_original_language_through_the_setter() {
    let item_id = Uuid::from_u128(0x59);
    for (member, expected) in [
        (r#","OriginalLanguage":"English""#, Some("en")),
        (r#","OriginalLanguage":"eng""#, Some("en")),
        (r#","OriginalLanguage":" ja ""#, Some(" ja ")),
        (r#","OriginalLanguage":"  ""#, None),
        (r#","OriginalLanguage":null"#, None),
        ("", None),
    ] {
        let written = update_and_capture(
            item_id,
            format!(
                r#"{{"Id":"{item_id}","Type":"Movie","MediaType":"Video","Name":"n"{member}}}"#
            ),
        )
        .await;
        assert_eq!(written.original_language.as_deref(), expected, "{member}");
    }
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
        refreshes: Arc::default(),
        provider_ids: Arc::default(),
        updated: Arc::default(),
        tree: Arc::new(tree),
        lock_probe: None,
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

/// `ItemUpdateController.UpdateItem`'s series walk: the series' name,
/// rating, custom rating and tag edit reach its seasons and their episodes
/// (every child here follows the series' ratings), and each child's own
/// `LockedFields` shields its `OfficialRating` and `Tags`.
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
            official_rating: Some("Old rating".into()),
            custom_rating: Some("Old custom".into()),
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

/// The last row `update_items` was given for `id`.
fn written(library: &OkLibrary, id: u128) -> BaseItemEntity {
    library
        .updated
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e.id == Uuid::from_u128(id).to_string())
        .cloned()
        .expect("row written")
}

/// A row of `kind` under `parent` rated `rating`.
fn rated(id: u128, kind: &str, parent: Uuid, rating: Option<&str>) -> BaseItemEntity {
    BaseItemEntity {
        official_rating: rating.map(ToOwned::to_owned),
        custom_rating: None,
        ..child(id, kind, parent, None)
    }
}

/// Upstream `ItemUpdateControllerTests.CreateSeries(seriesRating,
/// episodeRatings)`: a series (`0x5E`) and its season (`0x51`) both rated
/// `series_rating`, the season's episodes (`0xE0`, `0xE1`, …) rated
/// `episode_ratings`.
fn rated_series(series_rating: &str, episode_ratings: &[Option<&str>]) -> Tree {
    let series_id = Uuid::from_u128(0x5E);
    let season_id = Uuid::from_u128(0x51);
    let episodes = (0u128..)
        .zip(episode_ratings)
        .map(|(i, rating)| rated(0xE0 + i, "TV.Episode", season_id, *rating))
        .collect();
    Tree {
        root: Some(BaseItemEntity {
            is_folder: true,
            ..rated(0x5E, "TV.Series", series_id, Some(series_rating))
        }),
        children: std::collections::HashMap::from([
            (
                series_id,
                vec![rated(0x51, "TV.Season", series_id, Some(series_rating))],
            ),
            (season_id, episodes),
        ]),
        ..Tree::default()
    }
}

/// `UpdateItem_SeriesRatingUnchanged_KeepsEpisodeRatings` (jellyfin#18241):
/// saving a series without changing its rating leaves every child's alone.
#[tokio::test]
async fn a_series_edit_that_keeps_its_rating_leaves_the_children_alone() {
    let library = post_update(
        tree_library(
            Uuid::from_u128(0x5E),
            rated_series("TV-PG", &[Some("TV-PG"), Some("TV-14"), None]),
        ),
        r#"{"Overview":"New overview","OfficialRating":"TV-PG"}"#.to_owned(),
    )
    .await;
    assert_eq!(
        written(&library, 0x51).official_rating.as_deref(),
        Some("TV-PG")
    );
    assert_eq!(
        written(&library, 0xE1).official_rating.as_deref(),
        Some("TV-14")
    );
    assert_eq!(written(&library, 0xE2).official_rating, None);
}

/// `UpdateItem_SeriesRatingChanged_PropagatesOnlyToChildrenFollowingIt`: a
/// changed series rating reaches the season and the episodes that had the
/// old one or none, not an episode with its own or a locked one.
#[tokio::test]
async fn a_changed_series_rating_reaches_only_the_children_following_it() {
    let mut tree = rated_series("TV-PG", &[Some("TV-PG"), Some("TV-14"), None]);
    let season_id = Uuid::from_u128(0x51);
    tree.children.get_mut(&season_id).unwrap().push(rated(
        0xEF,
        "TV.Episode",
        season_id,
        Some("TV-PG"),
    ));
    tree.locked = std::collections::HashMap::from([(
        Uuid::from_u128(0xEF),
        vec![MetadataField::OfficialRating],
    )]);
    let library = post_update(
        tree_library(Uuid::from_u128(0x5E), tree),
        r#"{"OfficialRating":"TV-MA"}"#.to_owned(),
    )
    .await;
    for (id, expected) in [
        (0x51, "TV-MA"),
        (0xE0, "TV-MA"),
        (0xE1, "TV-14"),
        (0xE2, "TV-MA"),
        (0xEF, "TV-PG"),
    ] {
        assert_eq!(
            written(&library, id).official_rating.as_deref(),
            Some(expected),
            "{id:#x}"
        );
    }
}

/// `UpdateItem_SeriesCustomRatingChanged_PropagatesIt`: `CustomRating`
/// follows the same rule, and the unchanged official rating stays put.
#[tokio::test]
async fn a_changed_series_custom_rating_reaches_the_children_following_it() {
    let mut tree = rated_series("TV-PG", &[Some("TV-PG"), Some("TV-14"), None]);
    tree.children.get_mut(&Uuid::from_u128(0x51)).unwrap()[0].custom_rating = Some("TV-Y".into());
    let library = post_update(
        tree_library(Uuid::from_u128(0x5E), tree),
        r#"{"OfficialRating":"TV-PG","CustomRating":"XXX"}"#.to_owned(),
    )
    .await;
    assert_eq!(
        written(&library, 0x51).custom_rating.as_deref(),
        Some("XXX")
    );
    assert_eq!(
        written(&library, 0xE0).custom_rating.as_deref(),
        Some("TV-Y")
    );
    let open = written(&library, 0xE1);
    assert_eq!(open.custom_rating.as_deref(), Some("XXX"));
    assert_eq!(open.official_rating.as_deref(), Some("TV-14"));
}

/// A series' episodes follow their SEASON's change: a season with a
/// rating of its own keeps it, and so do the episodes that followed it.
#[tokio::test]
async fn series_episodes_follow_their_seasons_rating_change() {
    let mut tree = rated_series("TV-PG", &[Some("TV-Y7"), None]);
    tree.children.get_mut(&Uuid::from_u128(0x5E)).unwrap()[0].official_rating =
        Some("TV-Y7".into());
    let library = post_update(
        tree_library(Uuid::from_u128(0x5E), tree),
        r#"{"OfficialRating":"TV-MA"}"#.to_owned(),
    )
    .await;
    for (id, expected) in [(0x51, Some("TV-Y7")), (0xE0, Some("TV-Y7")), (0xE1, None)] {
        assert_eq!(
            written(&library, id).official_rating.as_deref(),
            expected,
            "{id:#x}"
        );
    }
}

/// `UpdateItem_SeasonRatingChanged_KeepsEpisodesWithTheirOwnRating`, and the
/// album walk by the same rule, matching the old rating case-insensitively.
#[tokio::test]
async fn season_and_album_rating_changes_reach_only_the_children_following_them() {
    for (kind, child_kind, previous, new, kids) in [
        (
            "TV.Season",
            "TV.Episode",
            "TV-PG",
            "TV-MA",
            [
                (Some("TV-PG"), Some("TV-MA")),
                (Some("TV-14"), Some("TV-14")),
                (None, Some("TV-MA")),
            ],
        ),
        (
            "Audio.MusicAlbum",
            "Audio.Audio",
            "PG-13",
            "R",
            [
                (Some("pg-13"), Some("R")),
                (Some("PG"), Some("PG")),
                (Some(""), Some("R")),
            ],
        ),
    ] {
        let parent = Uuid::from_u128(0x70);
        let tree = Tree {
            root: Some(BaseItemEntity {
                is_folder: true,
                ..rated(0x70, kind, parent, Some(previous))
            }),
            children: std::collections::HashMap::from([(
                parent,
                (0u128..)
                    .zip(&kids)
                    .map(|(i, (rating, _))| rated(0x71 + i, child_kind, parent, *rating))
                    .collect(),
            )]),
            ..Tree::default()
        };
        let library = post_update(
            tree_library(parent, tree),
            format!(r#"{{"OfficialRating":"{new}"}}"#),
        )
        .await;
        for (i, (_, expected)) in (0u128..).zip(&kids) {
            assert_eq!(
                written(&library, 0x71 + i).official_rating.as_deref(),
                *expected,
                "{kind} child {i}"
            );
        }
    }
}

/// `IHasDisplayOrder.DisplayOrder = request.DisplayOrder` (`:412-415`): a box
/// set's and a series' display order is written to the row as sent, a null
/// clears it, and no other kind keeps one; a series whose order changed
/// (compared case-insensitively, null as empty) queues a `FullRefresh` that
/// replaces its metadata (`:83-86,120-132`) — a box set's change does not.
#[tokio::test]
async fn a_display_order_edit_is_stored_and_a_series_one_refreshes_it() {
    let id = Uuid::from_u128(0xD0);
    for (kind, stored, sent, expected_data, refreshes) in [
        (
            "Movies.BoxSet",
            None,
            r#","DisplayOrder":"SortName""#,
            Some(r#"{"DisplayOrder":"SortName"}"#),
            false,
        ),
        (
            "Movies.BoxSet",
            Some(r#"{"DisplayOrder":"Default"}"#),
            "",
            None,
            false,
        ),
        (
            "TV.Series",
            Some(r#"{"DisplayOrder":"dvd"}"#),
            r#","DisplayOrder":"absolute""#,
            Some(r#"{"DisplayOrder":"absolute"}"#),
            true,
        ),
        // Case alone is no change; a null one against nothing is none either.
        (
            "TV.Series",
            Some(r#"{"DisplayOrder":"DVD"}"#),
            r#","DisplayOrder":"dvd""#,
            Some(r#"{"DisplayOrder":"dvd"}"#),
            false,
        ),
        ("TV.Series", None, "", None, false),
        // Clearing an order the series had is a change.
        (
            "TV.Series",
            Some(r#"{"DisplayOrder":"dvd"}"#),
            "",
            None,
            true,
        ),
        // A movie keeps no display order, whatever is posted.
        (
            "Movies.Movie",
            None,
            r#","DisplayOrder":"dvd""#,
            None,
            false,
        ),
    ] {
        let tree = Tree {
            root: Some(BaseItemEntity {
                type_: format!("MediaBrowser.Controller.Entities.{kind}"),
                is_folder: true,
                path: Some("/media/library/Item".to_owned()),
                data: stored.map(str::to_owned),
                ..base_item_entity(id)
            }),
            ..Tree::default()
        };
        let library = post_update(
            tree_library(id, tree),
            format!(r#"{{"Id":"{id}","Name":"X"{sent}}}"#),
        )
        .await;
        let written = library.updated.lock().unwrap().clone();
        let row = written.last().expect("the item is written");
        assert_eq!(
            row.data.as_deref(),
            expected_data,
            "{kind} {stored:?} + {sent:?}"
        );
        let queued = library.refreshes.lock().unwrap().clone();
        assert_eq!(
            queued.iter().map(|(item, _)| *item).collect::<Vec<_>>(),
            if refreshes { vec![id] } else { Vec::new() },
            "{kind} {stored:?} + {sent:?}"
        );
        if let Some((_, options)) = queued.first() {
            assert_eq!(
                options.metadata_refresh_mode,
                MetadataRefreshMode::FullRefresh
            );
            assert_eq!(options.image_refresh_mode, MetadataRefreshMode::FullRefresh);
            assert!(options.replace_all_metadata && !options.replace_all_images);
        }
    }
}

/// The season and album walks: episodes / tracks that followed the
/// parent's rating take the rating and tag edit, not the series name.
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
                official_rating: Some("Old rating".into()),
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
        // `ep.SeasonName = season.Name` (v12.2 `ItemUpdateController.cs:365`):
        // a season's episodes take its new name; an album's tracks have none.
        let season_name = (kind == "TV.Season").then_some("P");
        assert_eq!(kid.season_name.as_deref(), season_name, "{kind}");
    }
}

/// The fixture item stored as `kind` with `data`, edited with `fields`;
/// returns the row the handler wrote.
async fn edit_kind(kind: &str, data: Option<&str>, fields: &str) -> BaseItemEntity {
    let id = Uuid::from_u128(0x80);
    let tree = Tree {
        root: Some(BaseItemEntity {
            type_: format!("MediaBrowser.Controller.{kind}"),
            data: data.map(ToOwned::to_owned),
            ..base_item_entity(id)
        }),
        ..Tree::default()
    };
    let library = post_update(
        tree_library(id, tree),
        format!(r#"{{"Id":"{id}","Name":"N"{fields}}}"#),
    )
    .await;
    let written = library.updated.lock().unwrap().clone();
    written.into_iter().next().expect("item written")
}

/// The row's `Data` blob.
fn data_of(item: &BaseItemEntity) -> serde_json::Value {
    item.data
        .as_deref()
        .map_or(serde_json::Value::Null, |data| {
            serde_json::from_str(data).expect("json")
        })
}

/// The editor fields `UpdateItem` writes per kind (v12.2
/// `ItemUpdateController.cs:259-264, 423-437, 465-473`): a series' status,
/// air days, air time and runtime; an episode's special placement, kept in
/// `Data` compactly; a video's 3D format; a Live TV channel's height.
#[tokio::test]
async fn an_edit_writes_each_kinds_editor_fields() {
    let series = edit_kind(
        "Entities.TV.Series",
        None,
        r#","Status":"ended","AirDays":["Friday","Saturday"],"AirTime":"20:00","RunTimeTicks":"25000000000""#,
    )
    .await;
    let data = data_of(&series);
    assert_eq!(data["Status"], "Ended");
    assert_eq!(data["AirDays"], serde_json::json!(["Friday", "Saturday"]));
    assert_eq!(data["AirTime"], "20:00");
    assert_eq!(series.run_time_ticks, Some(25_000_000_000));
    // No air days sent: the stored ones stay; an empty status clears it.
    let series = edit_kind(
        "Entities.TV.Series",
        Some(r#"{"Status":"Ended","AirDays":["Friday"],"AirTime":"20:00"}"#),
        r#","Status":"""#,
    )
    .await;
    let data = data_of(&series);
    assert!(data.get("Status").is_none(), "{data}");
    assert_eq!(data["AirDays"], serde_json::json!(["Friday"]));
    assert_eq!(data["AirTime"], "20:00");

    let episode = edit_kind(
        "Entities.TV.Episode",
        None,
        r#","AirsBeforeSeasonNumber":"2","AirsBeforeEpisodeNumber":3,"AirsAfterSeasonNumber":null"#,
    )
    .await;
    let raw = episode.data.clone().expect("data");
    assert!(
        raw.contains(r#""AirsBeforeSeasonNumber":2"#)
            && raw.contains(r#""AirsBeforeEpisodeNumber":3"#),
        "compact, as the AiredDuringSeason filter matches them: {raw}"
    );
    assert!(!raw.contains("AirsAfterSeasonNumber"), "{raw}");

    let movie = edit_kind(
        "Entities.Movies.Movie",
        Some(r#"{"VideoType":"VideoFile"}"#),
        r#","Video3DFormat":"HalfSideBySide""#,
    )
    .await;
    assert_eq!(data_of(&movie)["Video3DFormat"], "HalfSideBySide");
    assert_eq!(data_of(&movie)["VideoType"], "VideoFile");

    let channel = edit_kind("LiveTv.LiveTvChannel", None, r#","Height":"1080""#).await;
    assert_eq!(channel.height, Some(1080));
    // A field of another kind is ignored.
    let movie = edit_kind(
        "Entities.Movies.Movie",
        None,
        r#","AirsBeforeSeasonNumber":2,"Status":"Ended""#,
    )
    .await;
    assert!(
        movie
            .data
            .as_deref()
            .is_none_or(|d| !d.contains("Airs") && !d.contains("Status"))
    );
}

/// A status `Enum.Parse` cannot read, on a series: upstream throws (500);
/// the request is refused with a 400 instead, and nothing is written. On any
/// other kind `Status` is never read, so the edit applies.
#[tokio::test]
async fn an_unreadable_series_status_is_a_bad_request() {
    let id = Uuid::from_u128(0x82);
    let send_to = |kind: &'static str| {
        let library = tree_library(
            id,
            Tree {
                root: Some(BaseItemEntity {
                    type_: format!("MediaBrowser.Controller.Entities.{kind}"),
                    ..base_item_entity(id)
                }),
                ..Tree::default()
            },
        );
        async move {
            let router = create_router(state_with_library(
                Arc::clone(&library),
                Arc::new(Mutex::new(Vec::new())),
            ));
            let response = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/Items/{id}"))
                        .header("X-Emby-Token", "valid")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(format!(
                            r#"{{"Id":"{id}","Name":"N","Status":"Cancelled","People":[{{"Name":"X"}}]}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            (
                response.status(),
                library.updated.lock().unwrap().len(),
                library.tree.people_written.lock().unwrap().len(),
            )
        }
    };
    assert_eq!(send_to("TV.Series").await, (StatusCode::BAD_REQUEST, 0, 0));
    assert_eq!(
        send_to("Movies.Movie").await,
        (StatusCode::NO_CONTENT, 1, 1)
    );
}

/// `request.People` replaces the item's credits before the save, each as
/// `new PersonInfo { Name, Role, Type }`; a nameless entry is dropped and an
/// absent `Type` is `Unknown`. Without the key nothing is written.
#[tokio::test]
async fn an_edit_writes_the_items_people() {
    let id = Uuid::from_u128(0x81);
    let library = post_update(
        tree_library(id, Tree::default()),
        format!(
            r#"{{"Id":"{id}","Name":"N","People":[{{"Name":"Al Pacino","Role":"Lt. Hanna","Type":"Actor"}},{{"Name":"Michael Mann","Type":"Director"}},{{"Role":"nobody"}},{{"Name":"Unsure"}}]}}"#
        ),
    )
    .await;
    let written = library.tree.people_written.lock().unwrap().clone();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].0, id.to_string());
    let credits: Vec<(String, Option<String>, Option<String>)> = written[0]
        .1
        .iter()
        .map(|p| (p.name.clone(), p.role.clone(), p.person_type.clone()))
        .collect();
    assert_eq!(
        credits,
        [
            (
                "Al Pacino".into(),
                Some("Lt. Hanna".into()),
                Some("Actor".into())
            ),
            ("Michael Mann".into(), None, Some("Director".into())),
            ("Unsure".into(), None, Some("Unknown".into())),
        ]
    );
    let library = post_update(
        tree_library(id, Tree::default()),
        format!(r#"{{"Id":"{id}","Name":"N"}}"#),
    )
    .await;
    assert!(library.tree.people_written.lock().unwrap().is_empty());
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

/// The editor's external ids are persisted and the write REPLACES the stored
/// set — C# `ItemUpdateController.UpdateItem` assigns them
/// (`item.SetProviderIds(request.ProviderIds)`, v12.4 lines 435-438), so a key
/// the client omitted is gone afterwards. The handler hands the request's whole
/// set over; the manager applies `TrySetProviderId` (trim, drop blank or
/// invalid ids, canonical names).
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
            refreshes: Arc::default(),
            provider_ids: recorded.clone(),
            updated: Arc::default(),
            tree: Arc::default(),
            lock_probe: None,
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
            ("Tvdb".to_owned(), String::new()),
        ],
        "the request's set is written as the whole new set"
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
            refreshes: Arc::default(),
            provider_ids: recorded.clone(),
            updated: Arc::default(),
            tree: Arc::default(),
            lock_probe: None,
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

/// A folder row of `kind` at `path` for the refresh tests.
fn folder_row(id: Uuid, kind: &str, path: Option<&str>, parent: Option<Uuid>) -> BaseItemEntity {
    let mut row = base_item_entity(id);
    kind.clone_into(&mut row.type_);
    row.is_folder = true;
    row.path = path.map(str::to_owned);
    row.parent_id = parent.map(|p| p.to_string());
    row
}

/// Posts `/Items/{id}/Refresh{query}` for the item row `root`, returning the
/// refreshes the library was asked to queue. What a refresh covers, and
/// whether it reaches the provider manager, is the library's
/// (`LibraryManager::queue_item_refresh`), so the handler queues none there.
async fn refresh_row(root: BaseItemEntity, query: &str) -> Vec<(Uuid, MetadataRefreshOptions)> {
    let id = Uuid::parse_str(&root.id).expect("id");
    let refreshes: RecordedRefreshes = Arc::default();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let options = RecordedOptions::default();
    let router = create_router(state_recording_options(
        Arc::new(OkLibrary {
            item_id: id,
            is_folder: root.is_folder,
            top_parent_id: None,
            refreshes: refreshes.clone(),
            provider_ids: Arc::default(),
            updated: Arc::default(),
            tree: Arc::new(Tree {
                root: Some(root),
                ..Tree::default()
            }),
            lock_probe: None,
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
    assert!(queued.lock().unwrap().is_empty());
    assert!(options.lock().unwrap().is_empty());
    refreshes.lock().unwrap().clone()
}

/// What jellyfin-web's refresh dialog sends for each choice
/// (`refreshdialog.js:76-90`), with "Replace existing images" ticked for the
/// two full refreshes — its PascalCase keys as the server binary's query-key
/// fold hands them to the router (`apps/ferrofin-server` lower-cases each
/// key's first letter before routing).
const SCAN_FOR_NEW: &str = "?recursive=true&imageRefreshMode=Default&metadataRefreshMode=Default&replaceAllImages=false&regenerateTrickplay=false&replaceAllMetadata=false";
const SEARCH_MISSING: &str = "?recursive=true&imageRefreshMode=FullRefresh&metadataRefreshMode=FullRefresh&replaceAllImages=true&regenerateTrickplay=true&replaceAllMetadata=false";
const REPLACE_ALL: &str = "?recursive=true&imageRefreshMode=FullRefresh&metadataRefreshMode=FullRefresh&replaceAllImages=true&regenerateTrickplay=false&replaceAllMetadata=true";

/// `POST /Items/{itemId}/Refresh` hands the item and the options
/// `ItemRefreshController` builds to the library's queued refresh
/// (`QueueRefresh`), for each of the dashboard's three choices and for a
/// request naming no mode: both are then `None`
/// (`ItemRefreshController.cs:64-65`) and nothing is forced. It answers
/// `204` without waiting for the refresh.
#[tokio::test]
async fn refresh_hands_the_item_and_the_request_options_to_the_library() {
    let folder_id = Uuid::from_u128(0x11B);
    let library = || {
        folder_row(
            folder_id,
            "MediaBrowser.Controller.Entities.CollectionFolder",
            Some("/config/root/default/Movies"),
            None,
        )
    };
    // "Scan for new and updated files" is the scan's own default refresh,
    // not automated (`ItemRefreshController.cs:86`).
    let scan_for_new = MetadataRefreshOptions {
        is_automated: false,
        ..MetadataRefreshOptions::default()
    };
    let search_missing = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_images: true,
        force_save: true,
        regenerate_trickplay: true,
        is_automated: false,
        ..MetadataRefreshOptions::default()
    };
    let replace_all = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        remove_old_metadata: true,
        force_save: true,
        is_automated: false,
        ..MetadataRefreshOptions::default()
    };
    let no_mode = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        is_automated: false,
        ..MetadataRefreshOptions::default()
    };
    for (query, expected) in [
        (SCAN_FOR_NEW, scan_for_new),
        (SEARCH_MISSING, search_missing),
        (REPLACE_ALL, replace_all),
        ("", no_mode),
    ] {
        assert_eq!(
            refresh_row(library(), query).await,
            vec![(folder_id, expected)],
            "{query}"
        );
    }
    let file = base_item_entity(Uuid::from_u128(0x5B));
    assert_eq!(
        refresh_row(file, "?metadataRefreshMode=FullRefresh")
            .await
            .iter()
            .map(|(id, options)| (*id, options.metadata_refresh_mode))
            .collect::<Vec<_>>(),
        [(Uuid::from_u128(0x5B), MetadataRefreshMode::FullRefresh)]
    );
}

/// `POST /Items/{itemId}/Refresh` for a missing item is a `404`; for the nil
/// id a `400`, as `GetItemById(Guid.Empty)` throws `ArgumentException`. Neither
/// queues.
#[rstest::rstest]
#[case::missing(Uuid::from_u128(0xC0DE), StatusCode::NOT_FOUND)]
#[case::nil(Uuid::nil(), StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn refresh_unresolved_item(#[case] item_id: Uuid, #[case] expected: StatusCode) {
    let queued = Arc::new(Mutex::new(Vec::new()));
    let router = create_router(state(Uuid::from_u128(0x5C), queued.clone()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}/Refresh"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
    assert!(queued.lock().unwrap().is_empty());
}

/// `POST /Items/{itemId}/ContentType` for a missing item is a `404`; for the
/// nil id a `400`, as `GetItemById(Guid.Empty)` throws `ArgumentException`.
#[rstest::rstest]
#[case::missing(Uuid::from_u128(0xFEED), StatusCode::NOT_FOUND)]
#[case::nil(Uuid::nil(), StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn content_type_unresolved_item(#[case] item_id: Uuid, #[case] expected: StatusCode) {
    let router = create_router(state(
        Uuid::from_u128(0x5D),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{item_id}/ContentType?contentType=movies"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
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
    // The fixture item's library type is unknown (a mixed library) and it has
    // no override: the selector offers Inherit, Movies and Shows
    // (`content_type_*` tests below cover the rest).
    assert_eq!(info.content_type_options.len(), 3);
    assert_eq!(info.content_type, None);
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

// ---- folder content-type overrides -------------------------------------------

/// A [`ServerConfigurationManager`] holding one configuration, which
/// `update_configuration` replaces.
#[derive(Default)]
struct RecordingConfig(Mutex<ServerConfiguration>);

#[async_trait]
impl ferrofin_traits::configuration::ServerConfigurationManager for RecordingConfig {
    fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        Arc::new(ferrofin_api::test_support::FakePaths)
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(self.0.lock().unwrap().clone()))
    }
    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        configuration.clone_into(&mut self.0.lock().unwrap());
        Ok(())
    }
    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_branding(
        &self,
        _branding: &ferrofin_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// The library for the content-type tests: `ITEM_ID` is `item`, its library's
/// type is `inherited` and its override `configured`.
fn content_type_library(
    item: BaseItemEntity,
    inherited: Option<CollectionType>,
    configured: Option<CollectionType>,
) -> Arc<OkLibrary> {
    Arc::new(OkLibrary {
        item_id: ITEM_ID,
        is_folder: item.is_folder,
        top_parent_id: None,
        refreshes: Arc::default(),
        provider_ids: Arc::default(),
        updated: Arc::default(),
        tree: Arc::new(Tree {
            root: Some(item),
            inherited_content_type: inherited,
            configured_content_type: configured,
            ..Tree::default()
        }),
        lock_probe: None,
    })
}

/// A row of `kind` at `path`.
fn item_at(kind: BaseItemKind, path: &str, is_folder: bool) -> BaseItemEntity {
    let mut item = base_item_entity(ITEM_ID);
    kind.stored_type_name()
        .expect("a stored kind")
        .clone_into(&mut item.type_);
    item.path = Some(path.to_owned());
    item.is_folder = is_folder;
    item
}

/// One request against `state`: (status, body bytes).
async fn send_to(state: AppState, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", "Token abc")
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

/// The editor's `(ContentType, option values)` for `item`.
async fn content_type_editor(
    item: BaseItemEntity,
    inherited: Option<CollectionType>,
    configured: Option<CollectionType>,
) -> (Option<CollectionType>, Vec<String>) {
    let state = state_with_library(
        content_type_library(item, inherited, configured),
        Arc::default(),
    );
    let (status, body) = send_to(state, "GET", &format!("/Items/{ITEM_ID}/MetadataEditor")).await;
    assert_eq!(status, StatusCode::OK);
    let info: MetadataEditorInfo = serde_json::from_slice(&body).expect("editor");
    let values = info
        .content_type_options
        .into_iter()
        .map(|option| option.value.unwrap_or_default())
        .collect();
    (info.content_type, values)
}

/// v12.2 `GetMetadataEditorInfo`: in a mixed library (no inherited type) a
/// folder is offered Inherit, Movies and Shows (#15797 added Movies); with an
/// override it shows it.
#[tokio::test]
async fn a_mixed_library_folder_is_offered_inherit_movies_and_shows() {
    let folder = || item_at(BaseItemKind::Series, "/media/mixed/Blade (1998)", true);
    assert_eq!(
        content_type_editor(folder(), None, None).await,
        (None, vec![String::new(), "movies".into(), "tvshows".into()])
    );
    assert_eq!(
        content_type_editor(folder(), None, Some(CollectionType::movies)).await,
        (
            Some(CollectionType::movies),
            vec![String::new(), "movies".into(), "tvshows".into()]
        )
    );
}

/// Where the library has a type, the selector appears only for an item that
/// already has an override, and only movies and shows libraries filter it.
#[tokio::test]
async fn a_typed_library_offers_the_selector_only_with_an_override() {
    let folder = || item_at(BaseItemKind::Folder, "/media/movies/Sets", true);
    assert_eq!(
        content_type_editor(folder(), Some(CollectionType::movies), None).await,
        (None, Vec::new())
    );
    assert_eq!(
        content_type_editor(
            folder(),
            Some(CollectionType::tvshows),
            Some(CollectionType::movies)
        )
        .await,
        (
            Some(CollectionType::movies),
            vec![String::new(), "movies".into(), "tvshows".into()]
        )
    );
    assert_eq!(
        content_type_editor(
            folder(),
            Some(CollectionType::music),
            Some(CollectionType::homevideos)
        )
        .await,
        (
            Some(CollectionType::homevideos),
            [
                "",
                "movies",
                "music",
                "tvshows",
                "homevideos",
                "musicvideos",
                "photos"
            ]
            .map(str::to_owned)
            .to_vec()
        )
    );
}

/// The option names go through the server's localization
/// (`GetLocalizedString`); the stub returns the phrase.
#[tokio::test]
async fn the_option_names_are_the_localized_phrases() {
    let state = state_with_library(
        content_type_library(
            item_at(BaseItemKind::Folder, "/media/x", true),
            Some(CollectionType::music),
            Some(CollectionType::music),
        ),
        Arc::default(),
    );
    let (_, body) = send_to(state, "GET", &format!("/Items/{ITEM_ID}/MetadataEditor")).await;
    let info: MetadataEditorInfo = serde_json::from_slice(&body).expect("editor");
    let names: Vec<_> = info
        .content_type_options
        .into_iter()
        .map(|option| option.name.unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        [
            "Inherit",
            "Movies",
            "Music",
            "Shows",
            "HomeVideos",
            "MusicVideos",
            "Photos"
        ]
    );
}

/// The guard: a virtual item, a library, a user view, a by-name item, a Live
/// TV channel or a channel's item never offers the selector, whatever the
/// library.
#[tokio::test]
async fn items_outside_a_library_are_offered_nothing() {
    let virtual_item = {
        let mut item = item_at(
            BaseItemKind::Episode,
            "/media/mixed/Show/missing.mkv",
            false,
        );
        item.is_virtual_item = true;
        item
    };
    let channel_item = {
        let mut item = item_at(BaseItemKind::Movie, "/media/mixed/x.mkv", false);
        item.channel_id = Some(Uuid::from_u128(0xC4).to_string());
        item
    };
    for item in [
        virtual_item,
        channel_item,
        item_at(
            BaseItemKind::CollectionFolder,
            "/config/root/default/Mixed",
            true,
        ),
        item_at(BaseItemKind::UserView, "/config/views/x", true),
        item_at(BaseItemKind::AggregateFolder, "/config/root", true),
        item_at(BaseItemKind::Person, "/config/metadata/People/A/Ada", false),
        item_at(BaseItemKind::MusicArtist, "/media/music/Artist", true),
        item_at(BaseItemKind::TvChannel, "/livetv/1", false),
    ] {
        let type_ = item.type_.clone();
        assert_eq!(
            content_type_editor(item, None, Some(CollectionType::movies)).await,
            (None, Vec::new()),
            "{type_}"
        );
    }
}

/// v12.2 `UpdateItemContentType`: the override is keyed on the item's
/// `ContainingFolderPath` — a folder's own path, a file's directory — and the
/// folder's previous entry is replaced; a blank type only clears it.
#[tokio::test]
async fn setting_a_content_type_keys_the_items_containing_folder() {
    async fn post(item: BaseItemEntity, config: &Arc<RecordingConfig>, value: &str) {
        let state = state_with_library_and_config(
            content_type_library(item, None, None),
            Arc::clone(config) as _,
        );
        let (status, _) = send_to(
            state,
            "POST",
            // jellyfin-web's spelling; the query binds ignoring case.
            &format!("/Items/{ITEM_ID}/ContentType?ContentType={value}"),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let entries = |config: &RecordingConfig| config.0.lock().unwrap().content_types.clone();
    let pair = |name: &str, value: &str| NameValuePair {
        name: Some(name.to_owned()),
        value: Some(value.to_owned()),
    };
    let config = Arc::new(RecordingConfig::default());
    let series = || item_at(BaseItemKind::Series, "/media/mixed/Blade (1998)", true);
    post(series(), &config, "tvshows").await;
    post(series(), &config, "movies").await;
    assert_eq!(
        entries(&config),
        vec![pair("/media/mixed/Blade (1998)", "movies")]
    );
    post(
        item_at(
            BaseItemKind::Movie,
            "/media/mixed/Heat (1995)/Heat.mkv",
            false,
        ),
        &config,
        "movies",
    )
    .await;
    assert_eq!(
        entries(&config),
        vec![
            pair("/media/mixed/Blade (1998)", "movies"),
            pair("/media/mixed/Heat (1995)", "movies"),
        ]
    );
    post(series(), &config, "").await;
    assert_eq!(
        entries(&config),
        vec![pair("/media/mixed/Heat (1995)", "movies")]
    );
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
