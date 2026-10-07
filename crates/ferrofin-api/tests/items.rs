//! Items domain integration tests: `/Items` query/read/counts/ancestors/delete,
//! `/Items/Root`, `/Items/Latest`, item extras (trailers, special features,
//! intros, critic reviews), and theme-media / file serving.
//!
//! Consolidated from `handler_success_paths.rs`, `batch4_handlers.rs`, and
//! `batch14_handlers.rs`. Two harnesses coexist here: the `ok_state` /
//! `state_with_providers` success-path harness (drives `create_router` inline),
//! and a `send()` helper backed by a `StubLibrary` that resolves a fixed item,
//! a root folder, and the various extra kinds.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeConfig, FakeMediaSources, FakeMusic, FakeProviders, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSystem, FakeUserData, minimal_base_item,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::entities::{ExtraType, ImageType};
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::querying::{AllThemeMediaResult, QueryResult, ThemeMediaResult};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    LibraryManager, UserManager, UserViewManager, VirtualFolderManager,
};
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

// Shared item / extra ids used by the `send()`-driven tests.
const ITEM_ID: Uuid = Uuid::from_u128(0xBEEF);
const ROOT_ID: Uuid = Uuid::from_u128(0x0F00);
const TRAILER_ID: Uuid = Uuid::from_u128(0xA1);
const SPECIAL_ID: Uuid = Uuid::from_u128(0xA2);
const SONG_ID: Uuid = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
const VIDEO_ID: Uuid = Uuid::from_u128(0x4444_4444_4444_4444_4444_4444_4444_4444);

/// Builds a minimal [`UserEntity`] carrying the given id + username; every other
/// field is a neutral zero value ([`UserEntity`] has no `Default`).
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

/// Builds a minimal [`BaseItemEntity`] with the given id + a fixed name; every
/// other field is `None`/`false`/empty ([`BaseItemEntity`] has no `Default`).
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
        type_: String::new(),
        unrated_type: None,
        width: None,
    }
}

/// Builds a minimal [`BaseItemEntity`] with the given id + name + kind.
fn item_entity(id: Uuid, name: &str, kind: BaseItemKind) -> BaseItemEntity {
    let type_ = serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(std::string::ToString::to_string))
        .unwrap_or_else(|| "Folder".to_owned());
    let mut item = base_item_entity(id);
    item.clean_name = Some(name.to_lowercase());
    item.name = Some(name.to_owned());
    item.type_ = type_;
    item
}

// ---- success-path harness (`ok_state`) ----------------------------------------

/// An [`AuthService`] that always authenticates as [`USER_ID`], so `RequireAuth`
/// yields an authenticated context in the success-path tests.
struct OkAuthService;

#[async_trait]
impl AuthService for OkAuthService {
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

/// An [`AuthorizationContext`] that resolves the same authenticated user.
struct OkAuthContext;

#[async_trait]
impl AuthorizationContext for OkAuthContext {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(user_entity(USER_ID, "alice")),
            client: Some("Wolphin".to_owned()),
            version: Some("1.0".to_owned()),
            device_id: Some("dev-1".to_owned()),
            device: Some("Test Device".to_owned()),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`UserManager`] whose `get_user_by_id` returns the fixed user.
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
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
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
    async fn change_password(
        &self,
        _user_id: Uuid,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
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
            policy: Some(ferrofin_model::users::UserPolicy {
                enable_content_deletion: true,
                ..Default::default()
            }),
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

/// The physical library-root `Folder` an item scanned by Jellyfin parents to.
const PHYSICAL_FOLDER_ID: Uuid = Uuid::from_u128(0xF01);
/// The `AggregateFolder` (`{data}/root`) that physical folder parents to.
const AGGREGATE_ID: Uuid = Uuid::from_u128(0xA66);
/// The library's `CollectionFolder` whose locations include the physical path.
const COLLECTION_FOLDER_ID: Uuid = Uuid::from_u128(0xC0F);
/// The `UserRootFolder` the collection folder parents to.
const USER_ROOT_ID: Uuid = Uuid::from_u128(0x500);

/// The `PlaylistsFolder` `CreateRootFolder` parents to the `AggregateFolder`
/// and registers as a virtual child (LibraryManager.cs:855-885).
const PLAYLISTS_FOLDER_ID: Uuid = Uuid::from_u128(0x9F1);

/// A playlist inside it — the chain that exercises the plug-in-folder exemption.
const PLAYLIST_ID: Uuid = Uuid::from_u128(0x9F2);
/// The physical folder's on-disk path (= one of the library's locations).
const PHYSICAL_PATH: &str = "/media/movies-real";

/// A [`LibraryManager`] returning one item from `query_items`, and resolving a
/// single known item id in `get_item_by_id` (any other id is `None`).
///
/// With `adopted_tree` set it also models a tree scanned by Jellyfin: the item
/// parents to a physical `Folder` under the `AggregateFolder`, while the
/// library's `CollectionFolder` parents to the `UserRootFolder`.
struct OkLibrary {
    item_id: Uuid,
    adopted_tree: bool,
    /// The last query `query_items` received.
    last_query: Arc<std::sync::Mutex<Option<InternalItemsQuery>>>,
    /// Every `delete_item` call: the id and whether its files were asked to go
    /// (`DeleteOptions::delete_file_location`).
    deleted: Arc<std::sync::Mutex<Vec<(Uuid, bool)>>>,
    /// When set, `delete_item` signals it has started and waits to be
    /// released before it records the delete.
    gate: Option<Arc<DeleteGate>>,
}

/// Holds a fake delete half way, so a test can drop the request meanwhile.
#[derive(Default)]
struct DeleteGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl LibraryManager for OkLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if self.adopted_tree && id == COLLECTION_FOLDER_ID {
            return Ok(Some(item_entity(
                COLLECTION_FOLDER_ID,
                "Movies",
                BaseItemKind::CollectionFolder,
            )));
        }
        Ok((id == self.item_id).then(|| {
            let mut item = base_item_entity(self.item_id);
            item.path = Some("/synthetic/movie.mkv".to_owned());
            item
        }))
    }
    async fn get_ancestors(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        if item_id == PLAYLIST_ID {
            // The playlists folder carries a real `Path` because the row
            // `CreateRootFolder` writes does: `TranslateParentItem` matches a
            // candidate on `PhysicalLocations.Contains(item.Path)`, and
            // `BaseItem.PhysicalLocations` is EMPTY for a pathless row
            // (BaseItem.cs:450-461), which would legitimately end the walk.
            let mut playlists = item_entity(
                PLAYLISTS_FOLDER_ID,
                "Playlists",
                BaseItemKind::PlaylistsFolder,
            );
            playlists.path = Some("/data/playlists".to_owned());
            return Ok(Some(vec![
                playlists,
                item_entity(AGGREGATE_ID, "root", BaseItemKind::AggregateFolder),
            ]));
        }
        if !self.adopted_tree {
            return Ok((item_id == self.item_id).then(Vec::new));
        }
        if item_id == self.item_id {
            let mut physical = item_entity(PHYSICAL_FOLDER_ID, "movies-real", BaseItemKind::Folder);
            physical.path = Some(PHYSICAL_PATH.to_owned());
            return Ok(Some(vec![
                physical,
                item_entity(AGGREGATE_ID, "root", BaseItemKind::AggregateFolder),
            ]));
        }
        if item_id == COLLECTION_FOLDER_ID {
            return Ok(Some(vec![item_entity(
                USER_ROOT_ID,
                "Media Folders",
                BaseItemKind::UserRootFolder,
            )]));
        }
        Ok(None)
    }
    async fn query_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        *self.last_query.lock().unwrap() = Some(query.clone());
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![base_item_entity(self.item_id)],
        ))
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
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn delete_item(&self, id: Uuid, options: &DeleteOptions) -> Result<(), ServiceError> {
        if let Some(gate) = &self.gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        self.deleted
            .lock()
            .expect("lock")
            .push((id, options.delete_file_location));
        Ok(())
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
        Ok(ferrofin_model::dto::ItemCounts {
            movie_count: 3,
            series_count: 1,
            ..ferrofin_model::dto::ItemCounts::default()
        })
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

/// A [`UserViewManager`] returning one folder view.
struct OkUserViews {
    item_id: Uuid,
}

#[async_trait]
impl UserViewManager for OkUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![base_item_entity(self.item_id)])
    }
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(user_id).await
    }
    async fn get_latest_items(
        &self,
        _query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        unimplemented!()
    }
}

/// A [`DtoService`] projecting each entity into a `BaseItemDto`.
struct OkDto;

fn entity_to_dto(item: &BaseItemEntity) -> BaseItemDto {
    BaseItemDto {
        id: Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil()),
        name: item.name.clone(),
        ..BaseItemDto::default()
    }
}

/// [`entity_to_dto`] plus an empty `Tags` list when the options carry the
/// `Tags` field — so the `AllowedTags ⇒ Tags` rule is observable on the wire.
fn entity_to_dto_with_fields(item: &BaseItemEntity, options: &DtoOptions) -> BaseItemDto {
    let mut dto = entity_to_dto(item);
    if options.contains_field(ferrofin_model::querying::ItemFields::Tags) {
        dto.tags = Some(Vec::new());
    }
    dto
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
        Ok(entity_to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|item| entity_to_dto_with_fields(item, options))
            .collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
    async fn can_delete(
        &self,
        _item: &BaseItemEntity,
        _user: &UserEntity,
    ) -> Result<bool, ServiceError> {
        Ok(true)
    }
}

/// A [`VirtualFolderManager`] listing one library whose locations include
/// [`PHYSICAL_PATH`] and whose item is [`COLLECTION_FOLDER_ID`].
struct OneLibrary;

#[async_trait]
impl VirtualFolderManager for OneLibrary {
    async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
        Ok(vec![VirtualFolderInfo {
            name: Some("Movies".to_owned()),
            locations: vec![PHYSICAL_PATH.to_owned()],
            item_id: Some(COLLECTION_FOLDER_ID.to_string()),
            ..VirtualFolderInfo::default()
        }])
    }
    async fn add_virtual_folder(
        &self,
        _name: &str,
        _collection_type: Option<ferrofin_model::entities::CollectionTypeOptions>,
        _options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn rename_virtual_folder(&self, _name: &str, _new: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn add_media_path(&self, _name: &str, _p: &MediaPathInfo) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_media_path(&self, _name: &str, _p: &MediaPathInfo) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn remove_media_path(&self, _name: &str, _path: &str) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_library_options(
        &self,
        _name: &str,
        _options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// Assembles an [`AppState`] wired for the success paths.
fn ok_state(item_id: Uuid) -> AppState {
    ok_state_with(OkLibrary {
        item_id,
        adopted_tree: false,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
    })
}

/// [`ok_state`] over an explicit library fake.
fn ok_state_with(library: OkLibrary) -> AppState {
    ok_state_with_users(library, Arc::new(OkUsers))
}

/// [`ok_state_with`] over a caller-chosen [`UserManager`] (a policy, say).
fn ok_state_with_users(library: OkLibrary, users: Arc<dyn UserManager>) -> AppState {
    ok_state_full(
        library,
        users,
        Arc::new(OkDto),
        (Arc::new(OkAuthContext), Arc::new(OkAuthService)),
    )
}

/// The request authentication a test state resolves callers with.
type Auth = (Arc<dyn AuthorizationContext>, Arc<dyn AuthService>);

/// [`ok_state_with_users`] over a caller-chosen DTO service and
/// authentication.
fn ok_state_full(
    library: OkLibrary,
    users: Arc<dyn UserManager>,
    dto: Arc<dyn DtoService>,
    (auth_context, auth_service): Auth,
) -> AppState {
    let item_id = library.item_id;
    AppState::new(
        Arc::new(library),
        users,
        Arc::new(OkUserViews { item_id }),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(ferrofin_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        dto,
        auth_context,
        auth_service,
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

/// Reads a response body into a JSON value.
async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---- extras harness (`send`) --------------------------------------------------

/// An auth pair that authenticates every request as [`USER_ID`].
struct StubAuth;

#[async_trait]
impl AuthService for StubAuth {
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
impl AuthorizationContext for StubAuth {
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

/// A [`LibraryManager`] that resolves [`ITEM_ID`], a [`ROOT_ID`] root folder, a
/// resume query result, and trailer / special-feature / theme-song / theme-video
/// extras keyed by the query's `extra_types`.
struct StubLibrary;

#[async_trait]
impl LibraryManager for StubLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id == ITEM_ID {
            let mut item = item_entity(ITEM_ID, "Movie", BaseItemKind::Movie);
            item.path = Some("/does/not/matter.mkv".to_owned());
            Ok(Some(item))
        } else {
            Ok(None)
        }
    }
    async fn get_user_root_folder(&self) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(Some(item_entity(
            ROOT_ID,
            "Media Folders",
            BaseItemKind::UserRootFolder,
        )))
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
        ))
    }
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if query.extra_types.contains(&ExtraType::Trailer) {
            Ok(vec![item_entity(
                TRAILER_ID,
                "Trailer",
                BaseItemKind::Trailer,
            )])
        } else if query.extra_types.contains(&ExtraType::BehindTheScenes) {
            Ok(vec![item_entity(SPECIAL_ID, "BTS", BaseItemKind::Video)])
        } else if query.extra_types.contains(&ExtraType::ThemeSong) {
            Ok(vec![minimal_base_item(SONG_ID, "Theme Song", "Audio")])
        } else if query.extra_types.contains(&ExtraType::ThemeVideo) {
            Ok(vec![minimal_base_item(VIDEO_ID, "Theme Video", "Video")])
        } else {
            Ok(Vec::new())
        }
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
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
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_items(
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
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ferrofin_traits::persistence::ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _s: ferrofin_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
}

/// A [`ProviderManager`] used by the theme-media route's owner resolution.
struct StubProviders;

#[async_trait]
impl ProviderManager for StubProviders {
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn queue_refresh(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
        _p: RefreshPriority,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_full_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn refresh_single_item(
        &self,
        _i: Uuid,
        _o: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        unimplemented!()
    }
    async fn save_image_from_url(
        &self,
        _i: Uuid,
        _u: &str,
        _t: ferrofin_model::entities::ImageType,
        _x: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn save_image(
        &self,
        _i: Uuid,
        _c: &[u8],
        _m: &str,
        _t: ferrofin_model::entities::ImageType,
        _x: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_available_remote_images(
        &self,
        _i: Uuid,
        _q: &ferrofin_model::providers::RemoteImageQuery,
    ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
        unimplemented!()
    }
    async fn get_remote_image_provider_info(
        &self,
        _i: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
        unimplemented!()
    }
    async fn save_metadata(&self, _i: Uuid, _u: ItemUpdateType) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError> {
        unimplemented!()
    }
    async fn get_metadata_options(
        &self,
        _i: Uuid,
    ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
        unimplemented!()
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
}

/// A [`UserViewManager`] returning one view.
struct StubUserViews;

#[async_trait]
impl UserViewManager for StubUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(vec![item_entity(
            ROOT_ID,
            "Movies",
            BaseItemKind::CollectionFolder,
        )])
    }
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views(user_id).await
    }
    async fn get_latest_items(
        &self,
        _query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        Ok(vec![(
            None,
            vec![item_entity(ITEM_ID, "Movie", BaseItemKind::Movie)],
        )])
    }
}

/// Builds an [`AppState`] wired with the extras stubs.
fn stub_state() -> AppState {
    AppState::new(
        Arc::new(StubLibrary),
        Arc::new(OkUsers),
        Arc::new(StubUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(ferrofin_api::test_support::FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(StubProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(OkDto),
        Arc::new(StubAuth),
        Arc::new(StubAuth),
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

/// Drives one request through the router and returns (status, body bytes).
async fn send(method: &str, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
    let router = create_router(stub_state());
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

#[tokio::test]
async fn items_returns_query_result_of_base_item_dto() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Items?startIndex=0&limit=10&recursive=true")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["TotalRecordCount"], 1);
    assert_eq!(json["StartIndex"], 0);
    assert_eq!(json["Items"][0]["Id"], item_id.simple().to_string());
    assert_eq!(json["Items"][0]["Name"], "Test Item");
}

/// `if (user.GetPreference(AllowedTags).Length != 0 && !fields.Contains(Tags))
///  fields = [.. fields, Tags];` (v12 ItemsController.cs:276-281): a user whose
/// policy whitelists tags gets `Tags` on every `/Items` row without asking.
#[tokio::test]
async fn items_adds_the_tags_field_for_a_user_with_allowed_tags() {
    use ferrofin_api::test_support::PolicyUsers;
    use ferrofin_model::users::UserPolicy;

    let item_id = Uuid::from_u128(0xABCD);
    let library = || OkLibrary {
        item_id,
        adopted_tree: false,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
    };
    let get = |state: AppState, uri: &'static str| async move {
        let response = create_router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    };
    let whitelisted = || {
        Arc::new(PolicyUsers(UserPolicy {
            allowed_tags: vec!["kids".to_owned()],
            ..UserPolicy::default()
        })) as Arc<dyn UserManager>
    };

    // Whitelist ⇒ Tags, unasked.
    let json = get(
        ok_state_with_users(library(), whitelisted()),
        "/Items?recursive=true",
    )
    .await;
    assert_eq!(json["Items"][0]["Tags"], serde_json::json!([]));
    // A policy with an empty whitelist ⇒ no Tags (the production shape, not
    // a missing policy).
    let json = get(
        ok_state_with_users(
            library(),
            Arc::new(PolicyUsers(UserPolicy::default())) as Arc<dyn UserManager>,
        ),
        "/Items?recursive=true",
    )
    .await;
    assert!(json["Items"][0].get("Tags").is_none(), "{json}");
    // Asked for explicitly ⇒ still present.
    let json = get(
        ok_state_with_users(library(), whitelisted()),
        "/Items?recursive=true&fields=Tags",
    )
    .await;
    assert_eq!(json["Items"][0]["Tags"], serde_json::json!([]));
}

#[tokio::test]
async fn item_by_id_returns_base_item_dto() {
    let item_id = Uuid::from_u128(0xABCD);
    let router = create_router(ok_state(item_id));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["Id"], item_id.simple().to_string());
    assert_eq!(json["Name"], "Test Item");
}

#[tokio::test]
async fn item_by_id_missing_is_404() {
    // The library knows only `item_id`; a different id resolves to `None` → 404.
    let router = create_router(ok_state(Uuid::from_u128(0xABCD)));
    let other = Uuid::from_u128(0xBEEF);
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{other}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A `GET /Items` request with a wide filter set is accepted (comma/pipe
/// parameters parse) and returns the query result.
#[tokio::test]
async fn get_items_with_filters_returns_query_result() {
    let item_id = Uuid::from_u128(0x51);
    let router = create_router(ok_state(item_id));
    let response = router
        .oneshot(
            Request::builder()
                .uri(
                    "/Items?includeItemTypes=Movie,Series&sortBy=SortName&sortOrder=Descending\
                     &filters=IsFavorite&genres=Action|Sci-Fi&years=1999,2001&isFavorite=true",
                )
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["TotalRecordCount"], 1);
    assert_eq!(json["Items"][0]["Id"], item_id.simple().to_string());
}

/// jellyfin-web's artist page filters by `AlbumArtistIds` (Albums),
/// `ContributingArtistIds` (Appears On) and `ExcludeArtistIds` (More Like
/// This), through both the query- and the path-scoped route. Each id set
/// must reach the repository query: a dropped one answers the Albums section
/// with every album in the library.
#[tokio::test]
async fn get_items_forwards_the_artist_id_filters() {
    let (album_artist, featured, excluded) = (
        Uuid::from_u128(0xA1),
        Uuid::from_u128(0xA2),
        Uuid::from_u128(0xA3),
    );
    let filters = format!(
        "includeItemTypes=MusicAlbum&recursive=true&albumArtistIds={album_artist}\
         &contributingArtistIds={featured},{excluded}&excludeArtistIds={excluded}"
    );
    for uri in [
        format!("/Items?{filters}"),
        format!("/Users/{USER_ID}/Items?{filters}"),
    ] {
        let library = OkLibrary {
            item_id: Uuid::from_u128(0x5A),
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: None,
        };
        let seen = Arc::clone(&library.last_query);
        let response = create_router(ok_state_with(library))
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let query = seen.lock().unwrap().take().expect("query_items ran");
        assert_eq!(query.album_artist_ids, [album_artist], "{uri}");
        assert_eq!(query.contributing_artist_ids, [featured, excluded], "{uri}");
        assert_eq!(query.exclude_artist_ids, [excluded], "{uri}");
        assert!(query.artist_ids.is_empty(), "{uri}");
    }
}

#[tokio::test]
async fn repeated_item_filters_keep_collections_and_first_scalars_on_both_routes() {
    use ferrofin_model::data::BaseItemKind;
    for path in ["/Items".to_owned(), format!("/Users/{USER_ID}/Items")] {
        let library = OkLibrary {
            item_id: Uuid::from_u128(0x5A),
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: None,
        };
        let seen = Arc::clone(&library.last_query);
        let response = create_router(ok_state_with(library))
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "{path}?includeItemTypes=Movie&INCLUDEITEMTYPES=Series\
                &genres=News%2CSport&GENRES=Drama&tags=one&TAGS=two\
                &limit=7&LIMIT=bad&recursive=true&Recursive=false\
                &searchTerm=a%2Cb&SEARCHTERM=ignored&userId={USER_ID}&USERID=bad\
                &genreIds={USER_ID}&GENREIDS={USER_ID}"
                    ))
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let query = seen.lock().unwrap().take().expect("query_items ran");
        assert_eq!(
            query.include_item_types,
            [BaseItemKind::Movie, BaseItemKind::Series]
        );
        assert_eq!(query.genres, ["News,Sport", "Drama"]);
        assert_eq!(query.tags, ["one", "two"]);
        assert_eq!(query.genre_ids, [USER_ID, USER_ID]);
        assert_eq!(query.limit, Some(7));
        assert!(query.recursive);
        assert_eq!(query.search_term.as_deref(), Some("a,b"));
    }
}

/// Wholphin's genre grid asks each genre for one random item that has a
/// backdrop (`imageTypes=Backdrop`) and builds the card's image URL from it;
/// an item without a backdrop yields a null URL that crashes the whole grid
/// (`ConcurrentHashMap.put(genreId, null)`). The image type set must reach the
/// repository query; unknown tokens drop like the binder's.
#[tokio::test]
async fn get_items_forwards_the_image_types_filter() {
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5B),
        adopted_tree: false,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
    };
    let seen = Arc::clone(&library.last_query);
    // Wholphin's request, verbatim apart from the ids.
    let uri = format!(
        "/Items?userId={USER_ID}&limit=1&recursive=true&parentId={}&fields=Genres\
         &includeItemTypes=MusicAlbum&imageTypes=Backdrop,Nonsense&sortBy=Random\
         &imageTypeLimit=1&genreIds={}&enableTotalRecordCount=false&enableImages=true",
        Uuid::from_u128(0x5C).simple(),
        Uuid::from_u128(0x5D).simple(),
    );
    let response = create_router(ok_state_with(library))
        .oneshot(
            Request::builder()
                .uri(&uri)
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let query = seen.lock().unwrap().take().expect("query_items ran");
    assert_eq!(query.image_types, [ImageType::Backdrop]);
}

/// A `GET /Items` with an unknown enum token is a `400`.
#[tokio::test]
async fn get_items_unknown_enum_token_is_dropped() {
    // ASP.NET's comma-delimited binder logs and DROPS unknown tokens rather
    // than failing the request — a 400 here broke real screens (jellyfin-web
    // sends spellings like IncludeItemTypes=LiveTVChannel for LiveTvChannel).
    let router = create_router(ok_state(Uuid::from_u128(0x52)));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Items?includeItemTypes=Nonsense")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// `GET /Items/Counts` returns the per-kind counts.
#[tokio::test]
async fn item_counts_returns_counts() {
    let router = create_router(ok_state(Uuid::from_u128(0x53)));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/Items/Counts")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json["MovieCount"], 3);
    assert_eq!(json["SeriesCount"], 1);
}

/// `GET /Items/{itemId}/Ancestors` returns an array (empty for a root item).
#[tokio::test]
async fn ancestors_of_root_item_is_empty_array() {
    let item_id = Uuid::from_u128(0x54);
    let router = create_router(ok_state(item_id));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Ancestors"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json.as_array().is_some_and(std::vec::Vec::is_empty));
}

/// `GET /Items/{itemId}/Ancestors` on a Jellyfin-scanned tree (the item
/// parents to a physical folder under the `AggregateFolder`): with a user in
/// scope — the explicit `userId`, or the authenticated caller, as C#
/// `RequestHelpers.GetUserId` resolves it — `TranslateParentItem` swaps the
/// physical folder for the user's view containing it, and the walk continues
/// up the view's own chain to the `UserRootFolder`.
#[tokio::test]
async fn ancestors_translate_physical_root_to_the_users_view() {
    let item_id = Uuid::from_u128(0x54);
    let state = ok_state_with(OkLibrary {
        item_id,
        adopted_tree: true,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
    })
    .with_virtual_folders(Arc::new(OneLibrary));
    let router = create_router(state);

    for uri in [
        format!("/Items/{item_id}/Ancestors"),
        format!("/Items/{item_id}/Ancestors?userId={USER_ID}"),
    ] {
        let translated = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(translated.status(), StatusCode::OK);
        let translated = json_body(translated).await;
        let ids: Vec<String> = translated
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["Id"].as_str().unwrap().to_ascii_uppercase())
            .collect();
        // `.simple()`: ids go out in Jellyfin's dashless "N" form
        // (`JsonGuidConverter`); this test is about the chain, not the format.
        assert_eq!(
            ids,
            vec![
                COLLECTION_FOLDER_ID
                    .simple()
                    .to_string()
                    .to_ascii_uppercase(),
                USER_ROOT_ID.simple().to_string().to_ascii_uppercase(),
            ],
            "{uri}: the view, then the user root: {translated}"
        );
    }
}

/// A plug-in folder under the `AggregateFolder` survives the translation: it is
/// a virtual child (`AddVirtualChild`), so it is IN the candidate set
/// `TranslateParentItem` searches, and its `PhysicalLocations` is `[Path]` — the
/// search therefore matches the folder against itself and the walk continues
/// through it. Leaving that group out of the candidate set answered a playlist's
/// ancestors with an EMPTY array where Jellyfin answers `[Playlists, root]`.
#[tokio::test]
async fn ancestors_keep_a_plugin_folder_and_the_physical_root() {
    let state = ok_state_with(OkLibrary {
        item_id: Uuid::from_u128(0x56),
        adopted_tree: true,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
    })
    .with_virtual_folders(Arc::new(OneLibrary));
    let router = create_router(state);

    let res = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{PLAYLIST_ID}/Ancestors?userId={USER_ID}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = json_body(res).await;
    let ids: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["Id"].as_str().unwrap().to_ascii_uppercase())
        .collect();
    assert_eq!(
        ids,
        vec![
            PLAYLISTS_FOLDER_ID
                .simple()
                .to_string()
                .to_ascii_uppercase(),
            AGGREGATE_ID.simple().to_string().to_ascii_uppercase(),
        ],
        "the playlists folder, then the physical root: {body}"
    );
}

/// `GET /Items/{itemId}/Ancestors` for a missing item is a `404`.
#[tokio::test]
async fn ancestors_of_missing_item_is_404() {
    let router = create_router(ok_state(Uuid::from_u128(0x55)));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{}/Ancestors", Uuid::from_u128(0xDEAD)))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `DELETE /Items/{itemId}` deletes an existing item (`204`).
#[tokio::test]
async fn delete_item_returns_204() {
    let item_id = Uuid::from_u128(0x56);
    let router = create_router(ok_state(item_id));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Items/{item_id}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// `DELETE /Items/{itemId}` for a missing item is a `404`.
#[tokio::test]
async fn delete_missing_item_is_404() {
    let router = create_router(ok_state(Uuid::from_u128(0x57)));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Items/{}", Uuid::from_u128(0xBEEF)))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `DELETE /Items?ids=...` deletes each listed item (`204`).
#[tokio::test]
async fn delete_items_batch_returns_204() {
    let item_id = Uuid::from_u128(0x58);
    let router = create_router(ok_state(item_id));
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/Items?ids={item_id}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

// ---- from batch4_handlers.rs --------------------------------------------------

#[tokio::test]
async fn root_folder_returns_root() {
    let (status, body) = send("GET", "/Items/Root", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let dto: BaseItemDto = serde_json::from_slice(&body).expect("dto");
    assert_eq!(dto.id, ROOT_ID);
}

#[tokio::test]
async fn local_trailers_returns_trailer_extra() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/LocalTrailers"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dtos: Vec<BaseItemDto> = serde_json::from_slice(&body).expect("dtos");
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, TRAILER_ID);
}

#[tokio::test]
async fn special_features_returns_display_extra() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/SpecialFeatures"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let dtos: Vec<BaseItemDto> = serde_json::from_slice(&body).expect("dtos");
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, SPECIAL_ID);
}

#[tokio::test]
async fn intros_are_empty() {
    let (status, body) = send("GET", &format!("/Items/{ITEM_ID}/Intros"), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert!(result.items.is_empty());
}

#[tokio::test]
async fn critic_reviews_are_empty() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/CriticReviews"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("result");
    assert!(result.items.is_empty());
}

#[tokio::test]
async fn latest_returns_flattened_items() {
    let (status, body) = send("GET", "/Items/Latest", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    let dtos: Vec<BaseItemDto> = serde_json::from_slice(&body).expect("dtos");
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, ITEM_ID);
}

// ---- from batch14_handlers.rs -------------------------------------------------

#[tokio::test]
async fn theme_songs_returns_song_extra() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ThemeSongs"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: ThemeMediaResult = serde_json::from_slice(&body).expect("theme media result");
    assert_eq!(result.owner_id, ITEM_ID);
    assert_eq!(result.result.items.len(), 1);
    assert_eq!(result.result.items[0].id, SONG_ID);
    assert_eq!(result.result.total_record_count, 1);
}

#[tokio::test]
async fn theme_videos_returns_video_extra() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ThemeVideos"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: ThemeMediaResult = serde_json::from_slice(&body).expect("theme media result");
    assert_eq!(result.result.items.len(), 1);
    assert_eq!(result.result.items[0].id, VIDEO_ID);
}

#[tokio::test]
async fn theme_media_combines_songs_and_videos() {
    let (status, body) = send(
        "GET",
        &format!("/Items/{ITEM_ID}/ThemeMedia"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: AllThemeMediaResult = serde_json::from_slice(&body).expect("all theme media");
    assert_eq!(result.theme_songs_result.result.items[0].id, SONG_ID);
    assert_eq!(result.theme_videos_result.result.items[0].id, VIDEO_ID);
    assert!(result.soundtrack_songs_result.result.items.is_empty());
}

#[tokio::test]
async fn theme_songs_missing_item_is_404() {
    let missing = Uuid::from_u128(0x9999_9999_9999_9999_9999_9999_9999_9999);
    let (status, _) = send(
        "GET",
        &format!("/Items/{missing}/ThemeSongs"),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn file_missing_item_is_404() {
    let missing = Uuid::from_u128(0x9999_9999_9999_9999_9999_9999_9999_9999);
    let (status, _) = send("GET", &format!("/Items/{missing}/File"), Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A [`DtoService`] that projects like [`OkDto`] but whose `CanDelete`
/// refuses the listed items — the per-user answer the delete endpoints ask
/// for (`BaseItem.CanDelete(user)`).
struct DenyingDto(Vec<Uuid>);

#[async_trait]
impl DtoService for DenyingDto {
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
        Ok(entity_to_dto(item))
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items.iter().map(entity_to_dto).collect())
    }
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        Ok(entity_to_dto(item))
    }
    async fn can_delete(
        &self,
        item: &BaseItemEntity,
        _user: &UserEntity,
    ) -> Result<bool, ServiceError> {
        Ok(!self
            .0
            .contains(&Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil())))
    }
}

/// A [`DtoService`] whose projected `CanDelete` is fixed and which does NOT
/// override `can_delete`, so the trait's default — the DTO's own field —
/// answers the delete endpoints.
struct DtoSays(Option<bool>);

#[async_trait]
impl DtoService for DtoSays {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        let mut dto = entity_to_dto(item);
        if options.contains_field(ferrofin_model::querying::ItemFields::CanDelete) {
            dto.can_delete = self.0;
        }
        Ok(dto)
    }
    async fn get_base_item_dtos(
        &self,
        _items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!()
    }
}

/// An [`AuthService`] whose token is authenticated but names no user and is
/// not an API key — a token whose user has gone.
struct UserlessAuthService;

#[async_trait]
impl AuthService for UserlessAuthService {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// Sends `DELETE uri` through a router over `state`.
async fn delete(state: AppState, uri: &str) -> StatusCode {
    create_router(state)
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The authenticated user every other test runs as.
fn alice() -> Auth {
    (Arc::new(OkAuthContext), Arc::new(OkAuthService))
}

/// The delete log a [`two_item_state`] library keeps.
type DeleteLog = Arc<std::sync::Mutex<Vec<(Uuid, bool)>>>;

/// A state over the adopted tree, so both `item_id` and
/// [`COLLECTION_FOLDER_ID`] resolve, with the given DTO service and
/// authentication — plus the library's delete log.
fn two_item_state(item_id: Uuid, dto: Arc<dyn DtoService>, auth: Auth) -> (AppState, DeleteLog) {
    let library = OkLibrary {
        item_id,
        adopted_tree: true,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
    };
    let deleted = Arc::clone(&library.deleted);
    (
        ok_state_full(library, Arc::new(OkUsers), dto, auth),
        deleted,
    )
}

/// `LibraryController.DeleteItem`: `!item.CanDelete(user)` is `401
/// Unauthorized access` and nothing is deleted; allowed, the item's rows are
/// deleted — and only them: upstream's `DeleteFileLocation = true` is an open
/// work item (`PLAN_ITEM_FILE_DELETION.md`), so the library is not asked to
/// delete files.
#[tokio::test]
async fn delete_item_asks_can_delete_and_deletes_the_rows_only() {
    let item_id = Uuid::from_u128(0xAB51);
    let (state, deleted) = two_item_state(item_id, Arc::new(OkDto), alice());
    assert_eq!(
        delete(state, &format!("/Items/{item_id}")).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(*deleted.lock().unwrap(), vec![(item_id, false)]);

    let (state, deleted) = two_item_state(item_id, Arc::new(DenyingDto(vec![item_id])), alice());
    assert_eq!(
        delete(state, &format!("/Items/{item_id}")).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(deleted.lock().unwrap().is_empty(), "nothing deleted");
}

/// With no `can_delete` override, the endpoints follow the DTO's own
/// `CanDelete` — the field jellyfin-web shows its "Delete" entry by — and an
/// absent one refuses.
#[tokio::test]
async fn the_default_can_delete_is_the_dtos_field() {
    let item_id = Uuid::from_u128(0xAB52);
    for (says, expected) in [
        (Some(true), StatusCode::NO_CONTENT),
        (Some(false), StatusCode::UNAUTHORIZED),
        (None, StatusCode::UNAUTHORIZED),
    ] {
        let (state, _) = two_item_state(item_id, Arc::new(DtoSays(says)), alice());
        assert_eq!(
            delete(state, &format!("/Items/{item_id}")).await,
            expected,
            "{says:?}"
        );
    }
}

/// `LibraryController.DeleteItems` is per item, in order: the ids before a
/// refused one are already deleted when the request answers `401`, and the
/// ids after it are never reached.
#[tokio::test]
async fn delete_items_stops_at_the_first_refused_id() {
    let item_id = Uuid::from_u128(0xAB53);
    let forbidden = COLLECTION_FOLDER_ID;
    let denying = || -> Arc<dyn DtoService> { Arc::new(DenyingDto(vec![forbidden])) };
    let (state, deleted) = two_item_state(item_id, denying(), alice());
    assert_eq!(
        delete(
            state,
            &format!("/Items?ids={item_id},{forbidden},{item_id}")
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(*deleted.lock().unwrap(), vec![(item_id, false)]);

    let (state, deleted) = two_item_state(item_id, denying(), alice());
    assert_eq!(
        delete(state, &format!("/Items?ids={forbidden},{item_id}")).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(deleted.lock().unwrap().is_empty());

    // A missing id stops the batch the same way, with a `404`.
    let (state, deleted) = two_item_state(item_id, Arc::new(OkDto), alice());
    let missing = Uuid::from_u128(0xBEEF);
    assert_eq!(
        delete(state, &format!("/Items?ids={item_id},{missing},{item_id}")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(*deleted.lock().unwrap(), vec![(item_id, false)]);
}

/// An API key has no user, so `CanDelete(user)` is not asked (upstream's
/// `user is not null && …`); a token whose user is gone is `404` on the
/// single delete and `401` on the batch, as upstream.
#[tokio::test]
async fn an_api_key_skips_the_check_and_a_userless_token_is_refused() {
    let item_id = Uuid::from_u128(0xAB54);
    for uri in [format!("/Items/{item_id}"), format!("/Items?ids={item_id}")] {
        let (state, deleted) = two_item_state(
            item_id,
            Arc::new(DenyingDto(vec![item_id])),
            (
                Arc::new(ferrofin_api::test_support::FakeAuthContext),
                Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
            ),
        );
        assert_eq!(delete(state, &uri).await, StatusCode::NO_CONTENT, "{uri}");
        assert_eq!(*deleted.lock().unwrap(), vec![(item_id, false)]);
    }
    for (uri, expected) in [
        (format!("/Items/{item_id}"), StatusCode::NOT_FOUND),
        (format!("/Items?ids={item_id}"), StatusCode::UNAUTHORIZED),
    ] {
        let (state, deleted) = two_item_state(
            item_id,
            Arc::new(OkDto),
            (
                Arc::new(ferrofin_api::test_support::FakeAuthContext),
                Arc::new(UserlessAuthService),
            ),
        );
        assert_eq!(delete(state, &uri).await, expected, "{uri}");
        assert!(deleted.lock().unwrap().is_empty());
    }
}

/// A client that goes away mid-delete does not stop the delete: the work
/// runs on its own task (upstream's controller ignores `RequestAborted`), so
/// the item is deleted even though the request future was dropped while the
/// library was deleting.
#[tokio::test]
async fn a_dropped_request_still_finishes_its_delete() {
    let item_id = Uuid::from_u128(0xAB55);
    for uri in [format!("/Items/{item_id}"), format!("/Items?ids={item_id}")] {
        let gate = Arc::new(DeleteGate::default());
        let library = OkLibrary {
            item_id,
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: Some(Arc::clone(&gate)),
        };
        let deleted = Arc::clone(&library.deleted);
        let state = ok_state_full(library, Arc::new(OkUsers), Arc::new(OkDto), alice());
        let request = tokio::spawn(async move { delete(state, &uri).await });
        gate.started.notified().await;
        request.abort();
        assert!(
            request.await.is_err_and(|e| e.is_cancelled()),
            "the request was dropped"
        );
        gate.release.notify_one();
        for _ in 0..200 {
            if !deleted.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(*deleted.lock().unwrap(), vec![(item_id, false)]);
    }
}
