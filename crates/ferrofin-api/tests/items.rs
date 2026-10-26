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
    ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
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

const SEASON_VIEW_ID: Uuid = Uuid::from_u128(0x5EA0_0001);
const SEASON_BROWSE_ID: Uuid = Uuid::from_u128(0x5EA0_0002);
/// The physical library-root `Folder` an item scanned by Jellyfin parents to.
const PHYSICAL_FOLDER_ID: Uuid = Uuid::from_u128(0xF01);
/// The `AggregateFolder` (`{data}/root`) that physical folder parents to.
const AGGREGATE_ID: Uuid = Uuid::from_u128(0xA66);
/// The library's `CollectionFolder` whose locations include the physical path.
const COLLECTION_FOLDER_ID: Uuid = Uuid::from_u128(0xC0F);
/// The `UserRootFolder` the collection folder parents to.
const USER_ROOT_ID: Uuid = Uuid::from_u128(0x500);

/// The `PlaylistsFolder` `CreateRootFolder` parents to the `AggregateFolder`
/// and registers as a virtual child (v12.2 LibraryManager.cs:1101-1150).
const PLAYLISTS_FOLDER_ID: Uuid = Uuid::from_u128(0x9F1);

/// A playlist inside it — the chain that exercises the plug-in-folder exemption.
const PLAYLIST_ID: Uuid = Uuid::from_u128(0x9F2);
/// The user's Playlists view, displaying [`PLAYLISTS_FOLDER_ID`].
const PLAYLISTS_VIEW_ID: Uuid = Uuid::from_u128(0x9F3);
/// A stale Playlists view whose display parent no longer exists.
const STALE_PLAYLISTS_VIEW_ID: Uuid = Uuid::from_u128(0x9F4);
/// A `tvshows` library's `CollectionFolder`.
const TV_LIBRARY_ID: Uuid = Uuid::from_u128(0xC1F);
/// A `boxsets` library's `CollectionFolder`.
const BOXSETS_LIBRARY_ID: Uuid = Uuid::from_u128(0xC2F);
/// A default-builder view that delegates to [`TV_LIBRARY_ID`].
const TV_VIEW_ID: Uuid = Uuid::from_u128(0xC3F);
/// A box set, whose own listing is its linked children.
const BOX_SET_ID: Uuid = Uuid::from_u128(0xB05);
/// The physical folder's on-disk path (= one of the library's locations).
const PHYSICAL_PATH: &str = "/media/movies-real";

/// A [`LibraryManager`] returning one item from `query_items`, and resolving a
/// single known item id in `get_item_by_id` (any other id is `None`).
///
/// With `adopted_tree` set it also models a tree scanned by Jellyfin: the item
/// parents to a physical `Folder` under the `AggregateFolder`, while the
/// library's `CollectionFolder` parents to the `UserRootFolder`.
#[derive(Default)]
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
    /// A Person item `get_item_by_id` also resolves, shared with the
    /// provider fake that refreshes it, and its images.
    person: SharedRow,
    person_images: Vec<ferrofin_traits::options::ItemImageInfo>,
    /// When non-empty, the page `query_items` answers with, in this order and
    /// out of [`PAGE_TOTAL`] rows, instead of the single `item_id` row.
    rows: Vec<BaseItemEntity>,
    /// The rows a `Name` + one-kind lookup (`get_item_list` / `get_item_ids`)
    /// finds, matched ignoring case.
    named: Vec<(BaseItemKind, &'static str, Uuid)>,
}

impl OkLibrary {
    /// The ids of the [`OkLibrary::named`] rows `query` names.
    fn named_ids(&self, query: &InternalItemsQuery) -> Vec<Uuid> {
        let Some(name) = query.name.as_deref() else {
            return Vec::new();
        };
        let ids = self
            .named
            .iter()
            .filter(|(kind, row, _)| {
                query.include_item_types.contains(kind) && row.eq_ignore_ascii_case(name.trim())
            })
            .map(|&(_, _, id)| id);
        ids.take(
            query
                .limit
                .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(0)),
        )
        .collect()
    }
}

/// The total record count `query_items` reports alongside [`OkLibrary::rows`].
const PAGE_TOTAL: i32 = 9;

/// Holds a fake delete half way, so a test can drop the request meanwhile.
#[derive(Default)]
struct DeleteGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl LibraryManager for OkLibrary {
    async fn get_item_images(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
        let is_person = self
            .person
            .lock()
            .expect("person")
            .as_ref()
            .is_some_and(|p| Uuid::parse_str(&p.id).ok() == Some(item_id));
        Ok(if is_person {
            self.person_images.clone()
        } else {
            Vec::new()
        })
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
        let person = self
            .person
            .lock()
            .expect("person")
            .clone()
            .filter(|p| Uuid::parse_str(&p.id).ok() == Some(id));
        if person.is_some() {
            return Ok(person);
        }
        if id == SEASON_VIEW_ID {
            let mut view = item_entity(SEASON_VIEW_ID, "Season view", BaseItemKind::UserView);
            view.data = Some(serde_json::json!({"DisplayParentId":SEASON_BROWSE_ID}).to_string());
            return Ok(Some(view));
        }
        if id == SEASON_BROWSE_ID {
            return Ok(Some(item_entity(
                SEASON_BROWSE_ID,
                "Season 1",
                BaseItemKind::Season,
            )));
        }
        if id == PLAYLIST_ID {
            return Ok(Some(item_entity(
                PLAYLIST_ID,
                "Playlist",
                BaseItemKind::Playlist,
            )));
        }
        if id == BOX_SET_ID {
            return Ok(Some(item_entity(
                BOX_SET_ID,
                "Box set",
                BaseItemKind::BoxSet,
            )));
        }
        if id == PLAYLISTS_VIEW_ID || id == STALE_PLAYLISTS_VIEW_ID {
            let shown = if id == PLAYLISTS_VIEW_ID {
                PLAYLISTS_FOLDER_ID
            } else {
                Uuid::from_u128(0xDEAD)
            };
            let mut view = item_entity(id, "Playlists", BaseItemKind::UserView);
            view.data = Some(
                serde_json::json!({"ViewType":"playlists","DisplayParentId":shown}).to_string(),
            );
            return Ok(Some(view));
        }
        if id == PLAYLISTS_FOLDER_ID {
            return Ok(Some(item_entity(
                PLAYLISTS_FOLDER_ID,
                "Playlists",
                BaseItemKind::PlaylistsFolder,
            )));
        }
        if id == TV_VIEW_ID {
            let mut view = item_entity(TV_VIEW_ID, "Shows", BaseItemKind::UserView);
            view.data = Some(serde_json::json!({"DisplayParentId":TV_LIBRARY_ID}).to_string());
            return Ok(Some(view));
        }
        for (library, name, collection_type) in [
            (TV_LIBRARY_ID, "Shows", "tvshows"),
            (BOXSETS_LIBRARY_ID, "Collections", "boxsets"),
        ] {
            if id == library {
                let mut folder = item_entity(library, name, BaseItemKind::CollectionFolder);
                folder.data =
                    Some(serde_json::json!({"CollectionType":collection_type}).to_string());
                return Ok(Some(folder));
            }
        }
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
        if !self.rows.is_empty() {
            return Ok(QueryResult::new(
                query.start_index,
                Some(PAGE_TOTAL),
                self.rows.clone(),
            ));
        }
        Ok(QueryResult::new(
            Some(0),
            Some(1),
            vec![base_item_entity(self.item_id)],
        ))
    }
    async fn get_item_ids(&self, query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        Ok(self.named_ids(query))
    }
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(self
            .named_ids(query)
            .into_iter()
            .map(base_item_entity)
            .collect())
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
        overview: item.overview.clone(),
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
    ) -> Result<Option<uuid::Uuid>, ServiceError> {
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
        ..Default::default()
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
    ok_state_providers(
        library,
        users,
        dto,
        (auth_context, auth_service),
        Arc::new(FakeProviders),
    )
}

/// [`ok_state_full`] over a caller-chosen [`ProviderManager`].
fn ok_state_providers(
    library: OkLibrary,
    users: Arc<dyn UserManager>,
    dto: Arc<dyn DtoService>,
    (auth_context, auth_service): Auth,
    providers: Arc<dyn ProviderManager>,
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
        providers,
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

/// A row the library fake serves and the provider fake rewrites.
type SharedRow = Arc<std::sync::Mutex<Option<BaseItemEntity>>>;

/// A [`ProviderManager`] recording the refreshes it is asked for. A refresh
/// writes [`FETCHED_BIO`] into `row`, as a provider that found the person
/// would.
#[derive(Default)]
struct RecordingProviders {
    row: SharedRow,
    /// When set, a refresh waits for a permit before it finishes.
    gate: Option<Arc<tokio::sync::Semaphore>>,
    /// A refresh fails instead.
    fail: bool,
    /// A refresh panics instead.
    panic: bool,
    refreshed: std::sync::Mutex<Vec<(Uuid, MetadataRefreshOptions)>>,
    finished: std::sync::Mutex<Vec<Uuid>>,
    queued: std::sync::Mutex<Vec<(Uuid, RefreshPriority)>>,
}

/// The overview a refresh by [`RecordingProviders`] writes, stamping the
/// row refreshed as the real one does.
const FETCHED_BIO: &str = "Fetched bio.";

#[async_trait]
impl ProviderManager for RecordingProviders {
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        _options: &MetadataRefreshOptions,
        priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        self.queued
            .lock()
            .expect("queued")
            .push((item_id, priority));
        Ok(())
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
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        self.refreshed
            .lock()
            .expect("refreshed")
            .push((item_id, options.clone()));
        if let Some(gate) = &self.gate {
            let _permit = gate.acquire().await.expect("gate");
        }
        assert!(!self.panic, "a provider bug");
        if self.fail {
            return Err(ServiceError::Backend("store down".into()));
        }
        if let Some(row) = self.row.lock().expect("row").as_mut() {
            row.overview = Some(FETCHED_BIO.to_owned());
            row.date_last_refreshed = Some(chrono::Utc::now());
        }
        self.finished.lock().expect("finished").push(item_id);
        Ok(ItemUpdateType::default())
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

/// `kind`'s stored `BaseItems.Type` name.
fn stored(kind: BaseItemKind) -> String {
    kind.stored_type_name().expect("stored name").to_owned()
}

/// A stored row of `kind`: `overview` and the last refresh as given.
fn person_row(
    id: Uuid,
    kind: BaseItemKind,
    overview: Option<&str>,
    refreshed_days_ago: Option<i64>,
) -> SharedRow {
    let mut row = item_entity(id, "Someone", kind);
    // As the database stores it, which is what the handler reads.
    row.type_ = stored(kind);
    row.overview = overview.map(str::to_owned);
    row.date_last_refreshed =
        refreshed_days_ago.map(|days| chrono::Utc::now() - chrono::TimeDelta::days(days));
    Arc::new(std::sync::Mutex::new(Some(row)))
}

/// A primary image, or none.
fn primary_images(primary: bool) -> Vec<ferrofin_traits::options::ItemImageInfo> {
    if !primary {
        return Vec::new();
    }
    vec![ferrofin_traits::options::ItemImageInfo {
        path: "/people/someone/folder.jpg".into(),
        image_type: ImageType::Primary,
        date_modified: chrono::Utc::now(),
        width: 1,
        height: 1,
        blur_hash: None,
    }]
}

/// A library serving `row` with `images`, and a state over `providers`.
fn person_state(
    row: &SharedRow,
    images: Vec<ferrofin_traits::options::ItemImageInfo>,
    providers: Arc<RecordingProviders>,
) -> AppState {
    ok_state_providers(
        OkLibrary {
            item_id: Uuid::from_u128(0xABCD),
            person: Arc::clone(row),
            person_images: images,
            ..Default::default()
        },
        Arc::new(OkUsers),
        Arc::new(OkDto),
        (Arc::new(OkAuthContext), Arc::new(OkAuthService)),
        providers,
    )
}

/// The item page on `uri`: its status and its `Overview`.
async fn get_person(state: AppState, uri: &str) -> (StatusCode, serde_json::Value) {
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
    let status = response.status();
    let overview = if status == StatusCode::OK {
        json_body(response).await["Overview"].clone()
    } else {
        serde_json::Value::Null
    };
    (status, overview)
}

/// C# `UserLibraryController.RefreshOnDemandIfNeeded`: a person shown without
/// an overview or without a primary image (`!IsNullOrWhiteSpace(Overview) &&
/// HasImage(Primary)` is false), and not refreshed for three days, is
/// refreshed in full (metadata and images, saved) before its page is
/// answered, on `/Items/{id}` and the legacy `/Users/{userId}/Items/{id}`.
/// The page shows what the refresh wrote.
#[rstest::rstest]
#[case::no_metadata("/Items/{id}", None, false, Some(4))]
#[case::legacy_route("/Users/{user}/Items/{id}", None, false, Some(4))]
#[case::overview_without_image("/Items/{id}", Some("A bio."), false, Some(4))]
#[case::image_without_overview("/Items/{id}", None, true, Some(4))]
#[case::blank_overview_with_image("/Items/{id}", Some("  "), true, Some(4))]
#[case::never_refreshed("/Items/{id}", None, false, None)]
#[tokio::test]
async fn a_person_without_metadata_is_refreshed_before_its_page(
    #[case] route: &str,
    #[case] overview: Option<&str>,
    #[case] primary_image: bool,
    #[case] refreshed_days_ago: Option<i64>,
) {
    let person = Uuid::new_v4();
    let row = person_row(person, BaseItemKind::Person, overview, refreshed_days_ago);
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        ..RecordingProviders::default()
    });
    let state = person_state(&row, primary_images(primary_image), Arc::clone(&providers));
    let uri = route
        .replace("{id}", &person.to_string())
        .replace("{user}", &USER_ID.to_string());
    let (status, shown) = get_person(state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown, FETCHED_BIO, "the page shows the refreshed row");
    let refreshed = providers.refreshed.lock().expect("refreshed").clone();
    assert_eq!(refreshed.len(), 1, "{refreshed:?}");
    let (id, options) = &refreshed[0];
    assert_eq!(*id, person);
    assert_eq!(
        options.metadata_refresh_mode,
        MetadataRefreshMode::FullRefresh
    );
    assert_eq!(options.image_refresh_mode, MetadataRefreshMode::FullRefresh);
    assert!(options.force_save);
    assert!(providers.queued.lock().expect("queued").is_empty());
}

/// A person with an overview and a primary image (whenever it was last
/// refreshed), or refreshed in the last three days, is not refreshed;
/// neither is anything but a person.
#[rstest::rstest]
#[case::has_metadata(Some("A bio."), true, None, BaseItemKind::Person)]
#[case::refreshed_lately(None, false, Some(2), BaseItemKind::Person)]
#[case::not_a_person(None, false, None, BaseItemKind::Movie)]
#[tokio::test]
async fn a_person_with_metadata_or_refreshed_lately_is_not_refreshed(
    #[case] overview: Option<&str>,
    #[case] primary_image: bool,
    #[case] refreshed_days_ago: Option<i64>,
    #[case] kind: BaseItemKind,
) {
    let id = Uuid::new_v4();
    let row = person_row(id, kind, overview, refreshed_days_ago);
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        ..RecordingProviders::default()
    });
    let state = person_state(&row, primary_images(primary_image), Arc::clone(&providers));
    let (status, shown) = get_person(state, &format!("/Items/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown, serde_json::json!(overview));
    assert!(providers.refreshed.lock().expect("refreshed").is_empty());
    assert!(providers.queued.lock().expect("queued").is_empty());
}

/// A refresh still running after the three seconds the page waits for is
/// left to finish: the page is answered without it, and no second refresh
/// is queued (upstream cancels it and queues one at high priority; a dropped
/// refresh could stop between its writes).
#[tokio::test(start_paused = true)]
async fn a_slow_on_demand_refresh_finishes_after_the_page() {
    let person = Uuid::new_v4();
    let row = person_row(person, BaseItemKind::Person, None, None);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        gate: Some(Arc::clone(&gate)),
        ..RecordingProviders::default()
    });
    let state = person_state(&row, Vec::new(), Arc::clone(&providers));
    let started = tokio::time::Instant::now();
    let (status, shown) = get_person(state, &format!("/Items/{person}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        shown,
        serde_json::Value::Null,
        "answered without the refresh"
    );
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(3));
    assert!(providers.finished.lock().expect("finished").is_empty());
    gate.add_permits(1);
    wait_for_finish(&providers).await;
    assert_eq!(*providers.finished.lock().expect("finished"), [person]);
    assert!(providers.queued.lock().expect("queued").is_empty());
}

/// While a person's refresh runs, another view of it is answered at once and
/// starts no second refresh.
#[tokio::test(start_paused = true)]
async fn a_second_view_starts_no_second_refresh() {
    let person = Uuid::new_v4();
    let row = person_row(person, BaseItemKind::Person, None, None);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        gate: Some(Arc::clone(&gate)),
        ..RecordingProviders::default()
    });
    let uri = format!("/Items/{person}");
    let state = person_state(&row, Vec::new(), Arc::clone(&providers));
    assert_eq!(get_person(state.clone(), &uri).await.0, StatusCode::OK);
    let started = tokio::time::Instant::now();
    assert_eq!(get_person(state, &uri).await.0, StatusCode::OK);
    assert_eq!(started.elapsed(), std::time::Duration::ZERO);
    assert_eq!(providers.refreshed.lock().expect("refreshed").len(), 1);
    gate.add_permits(1);
    wait_for_finish(&providers).await;
}

/// A client that hangs up while its page waits leaves the refresh to finish,
/// with nothing queued.
#[tokio::test(start_paused = true)]
async fn a_hang_up_leaves_the_refresh_to_finish() {
    let person = Uuid::new_v4();
    let row = person_row(person, BaseItemKind::Person, None, None);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        gate: Some(Arc::clone(&gate)),
        ..RecordingProviders::default()
    });
    let page = create_router(person_state(&row, Vec::new(), Arc::clone(&providers))).oneshot(
        Request::builder()
            .uri(format!("/Items/{person}"))
            .header("X-Emby-Token", "valid")
            .body(Body::empty())
            .unwrap(),
    );
    let hung_up = tokio::time::timeout(std::time::Duration::from_secs(1), page).await;
    assert!(hung_up.is_err(), "the page was still waiting");
    gate.add_permits(1);
    wait_for_finish(&providers).await;
    assert_eq!(
        row.lock()
            .expect("row")
            .as_ref()
            .unwrap()
            .overview
            .as_deref(),
        Some(FETCHED_BIO)
    );
    assert!(providers.queued.lock().expect("queued").is_empty());
}

/// A refresh that panics fails the page instead of hanging it.
#[tokio::test]
async fn a_panicking_on_demand_refresh_fails_the_page() {
    let person = Uuid::new_v4();
    let row = person_row(person, BaseItemKind::Person, None, None);
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        panic: true,
        ..RecordingProviders::default()
    });
    let state = person_state(&row, Vec::new(), providers);
    let (status, _) = get_person(state, &format!("/Items/{person}")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// Waits until a gated refresh has finished.
async fn wait_for_finish(providers: &RecordingProviders) {
    for _ in 0..100 {
        if !providers.finished.lock().expect("finished").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(providers.finished.lock().expect("finished").len(), 1);
}

/// A refresh that fails within the wait fails the page, as an exception
/// escaping C# `RefreshMetadata` does (a provider's failure is counted inside
/// the refresh in both, so this is the store failing).
#[tokio::test]
async fn a_failed_on_demand_refresh_fails_the_page() {
    let person = Uuid::new_v4();
    let row = person_row(person, BaseItemKind::Person, None, None);
    let providers = Arc::new(RecordingProviders {
        row: Arc::clone(&row),
        fail: true,
        ..RecordingProviders::default()
    });
    let state = person_state(&row, Vec::new(), providers);
    let (status, _) = get_person(state, &format!("/Items/{person}")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
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
        ..Default::default()
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

/// An empty query value is `null` (`ConvertEmptyStringToNull`), which
/// `CheckModel` rejects only for a member whose C# type is non-nullable —
/// `GetItems`' `bool enableTotalRecordCount = true` — and accepts for a
/// nullable one (`bool? isFavorite`). Measured on a live Jellyfin 12.2.
#[tokio::test]
async fn an_empty_value_is_rejected_only_for_a_non_nullable_member() {
    let item_id = Uuid::from_u128(0xABCD);
    for (query, expected) in [
        ("enableTotalRecordCount=", StatusCode::BAD_REQUEST),
        ("ENABLETOTALRECORDCOUNT=%20", StatusCode::BAD_REQUEST),
        ("enableTotalRecordCount=False", StatusCode::OK),
        ("isFavorite=", StatusCode::OK),
        ("limit=&recursive=", StatusCode::OK),
    ] {
        let response = create_router(ok_state(item_id))
            .oneshot(
                Request::builder()
                    .uri(format!("/Items?{query}"))
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{query}");
    }
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

/// The query `GET /Items?{params}` hands the library, and the status it
/// answered with.
async fn items_query(params: &str) -> (StatusCode, Option<InternalItemsQuery>) {
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5A),
        ..Default::default()
    };
    let seen = Arc::clone(&library.last_query);
    let response = create_router(ok_state_with(library))
        .oneshot(
            Request::builder()
                .uri(format!("/Items?{params}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let query = seen.lock().unwrap().take();
    (response.status(), query)
}

/// v12.2 `ItemsController.GetItems` (`ItemsController.cs:263-274`): an API key
/// that names no `userId` runs the query with no user, unscoped. It was a
/// `400` ("no user for request").
#[tokio::test]
async fn an_api_key_lists_items_with_no_user() {
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5B),
        ..Default::default()
    };
    let seen = Arc::clone(&library.last_query);
    let state = ok_state_full(
        library,
        Arc::new(OkUsers),
        Arc::new(OkDto),
        (
            Arc::new(ferrofin_api::test_support::FakeAuthContext),
            Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
        ),
    );
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Items?recursive=true&includeItemTypes=Movie")
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let query = seen.lock().unwrap().take().expect("the query ran");
    assert!(query.user.is_none(), "an API key has no user");
}

/// The filters v12.2 `GetItems` copies straight onto the query
/// (ItemsController.cs:396-430, 491-525): `seriesStatus` (jellyfin-web's TV
/// Status filter), `personTypes`, `isPlaceHolder` and `hasOfficialRating`, and
/// the two official ratings as `GetRatingScore` scores. A blank or unknown
/// rating sets no bound.
#[tokio::test]
async fn get_items_forwards_the_rating_status_and_placeholder_filters() {
    use ferrofin_model::entities::SeriesStatus;
    use ferrofin_model::entities_media::ParentalRatingScore;
    let (status, query) = items_query(
        "recursive=true&seriesStatus=Continuing,ended&minOfficialRating=PG\
         &maxOfficialRating=pg-13&personTypes=Actor,Director&isPlaceHolder=true\
         &hasOfficialRating=false",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let query = query.expect("query_items ran");
    assert_eq!(
        query.series_statuses,
        [SeriesStatus::Continuing, SeriesStatus::Ended]
    );
    assert_eq!(
        query.min_parental_rating,
        Some(ParentalRatingScore::new(10, None))
    );
    assert_eq!(
        query.max_parental_rating,
        Some(ParentalRatingScore::new(13, None))
    );
    assert_eq!(query.person_types, ["Actor", "Director"]);
    assert_eq!(query.is_place_holder, Some(true));
    assert_eq!(query.has_official_rating, Some(false));

    for params in [
        "minOfficialRating=%20&maxOfficialRating=",
        "minOfficialRating=Unrated-Nonsense&maxOfficialRating=XYZ",
    ] {
        let (status, query) = items_query(&format!("recursive=true&{params}")).await;
        assert_eq!(status, StatusCode::OK, "{params}");
        let query = query.expect("query_items ran");
        assert_eq!(query.min_parental_rating, None, "{params}");
        assert_eq!(query.max_parental_rating, None, "{params}");
    }
    let (status, _) = items_query("recursive=true&isPlaceHolder=maybe").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The query `GET /Items?{params}` hands the library when `named` are the
/// by-name rows and albums its lookups can find.
async fn items_query_with_named(
    params: &str,
    named: Vec<(BaseItemKind, &'static str, Uuid)>,
) -> InternalItemsQuery {
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5A),
        named,
        ..Default::default()
    };
    let seen = Arc::clone(&library.last_query);
    let response = create_router(ok_state_with(library))
        .oneshot(
            Request::builder()
                .uri(format!("/Items?{params}"))
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{params}");
    seen.lock().unwrap().take().expect("query_items ran")
}

/// `artists`, `albums` and `studios` name the items whose ids replace
/// `artistIds`, `albumIds` and `studioIds` (v12.2 ItemsController.cs:527-577).
/// A name nothing has takes the no-match id instead of a minted row, so it
/// matches nothing; an absent or blank list leaves the id filter as it was.
#[tokio::test]
async fn get_items_resolves_the_artist_album_and_studio_names() {
    let (artist, album, studio, kept) = (
        Uuid::from_u128(0xC1),
        Uuid::from_u128(0xC2),
        Uuid::from_u128(0xC3),
        Uuid::from_u128(0xC4),
    );
    let named = vec![
        (BaseItemKind::MusicArtist, "Test Artist", artist),
        (BaseItemKind::MusicAlbum, "Middle Years", album),
        (BaseItemKind::Studio, "Acme Pictures", studio),
    ];
    let query = items_query_with_named(
        &format!(
            "recursive=true&artistIds={kept}&artists=test%20artist|Nobody\
             &albumIds={kept}&albums=Middle%20Years&studioIds={kept}&studios=Acme%20Pictures|Nope"
        ),
        named.clone(),
    )
    .await;
    assert_eq!(query.artist_ids, [artist, Uuid::nil()]);
    assert_eq!(query.album_ids, [album]);
    assert_eq!(query.studio_ids, [studio, Uuid::nil()]);

    // A repeated key joins with the declared pipe delimiter.
    let repeated = items_query_with_named(
        "recursive=true&studios=Acme%20Pictures&studios=Nope",
        named.clone(),
    )
    .await;
    assert_eq!(repeated.studio_ids, [studio, Uuid::nil()]);

    let unknown = items_query_with_named("recursive=true&albums=Nope", named.clone()).await;
    assert_eq!(
        unknown.album_ids,
        [Uuid::nil()],
        "an unknown album matches nothing"
    );

    let blank = items_query_with_named(
        &format!("recursive=true&studioIds={kept}&studios=&artists=%20"),
        named,
    )
    .await;
    assert_eq!(blank.studio_ids, [kept]);
    assert!(blank.artist_ids.is_empty());
}

/// An artist's albums asked for with no `sortBy` come newest first, then by
/// name (v12.2 ItemsController.cs:579-586), whether the artist is named by
/// id or by name. Any requested sort, `Default` included, any other type set
/// and a request without an artist keep their own order.
#[tokio::test]
async fn get_items_orders_an_artists_albums_newest_first_by_default() {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    let artist = Uuid::from_u128(0xC1);
    let named = vec![(BaseItemKind::MusicArtist, "Test Artist", artist)];
    let newest_first = [
        (ItemSortBy::ProductionYear, SortOrder::Descending),
        (ItemSortBy::SortName, SortOrder::Ascending),
    ];
    for params in [
        format!("recursive=true&artistIds={artist}&includeItemTypes=MusicAlbum"),
        "recursive=true&artists=Test%20Artist&includeItemTypes=MusicAlbum".to_owned(),
        // A sort order with no sort key is no requested sort (`GetOrderBy`).
        format!(
            "recursive=true&artistIds={artist}&includeItemTypes=MusicAlbum&sortOrder=Ascending"
        ),
    ] {
        let query = items_query_with_named(&params, named.clone()).await;
        assert_eq!(query.order_by, newest_first, "{params}");
    }
    for (params, expected) in [
        (
            format!("recursive=true&artistIds={artist}&includeItemTypes=MusicAlbum&sortBy=Default"),
            vec![(ItemSortBy::Default, SortOrder::Ascending)],
        ),
        (
            format!("recursive=true&artistIds={artist}&includeItemTypes=MusicAlbum,Audio"),
            Vec::new(),
        ),
        (
            "recursive=true&includeItemTypes=MusicAlbum".to_owned(),
            Vec::new(),
        ),
    ] {
        let query = items_query_with_named(&params, named.clone()).await;
        assert_eq!(query.order_by, expected, "{params}");
    }
    // A search still leads with SortName, over the default.
    let searched = items_query_with_named(
        &format!("recursive=true&artistIds={artist}&includeItemTypes=MusicAlbum&searchTerm=light"),
        named,
    )
    .await;
    assert_eq!(
        searched.order_by,
        [
            (ItemSortBy::SortName, SortOrder::Ascending),
            newest_first[0],
            newest_first[1]
        ]
    );
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
            ..Default::default()
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

/// jellyfin-web's Playlists page browses the Playlists view. Measured on
/// Jellyfin 12.2 with a user: the view and the playlists folder each list
/// every playlist the user may open — the ones made in the app and the
/// playlist files the library scan found — whatever type was asked, so:
/// - untyped, the listing becomes recursive playlists from the user root
///   (post-filtered per user), with no linked-ancestor constraint;
/// - `IncludeItemTypes=Playlist` re-roots to the user root the same way,
///   since the view's collection type is the target's own;
/// - the playlists folder lists them too, whatever was asked — except a
///   `[BoxSet]` browse, which re-roots like any other parent.
#[tokio::test]
async fn playlists_view_browses_list_its_playlists() {
    use ferrofin_model::data::BaseItemKind;
    async fn query_for(params: &str) -> InternalItemsQuery {
        let library = OkLibrary {
            item_id: Uuid::from_u128(0x5A),
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: None,
            ..Default::default()
        };
        let seen = Arc::clone(&library.last_query);
        let response = create_router(ok_state_with(library))
            .oneshot(
                Request::builder()
                    .uri(format!("/Items?{params}"))
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{params}");
        seen.lock().unwrap().take().expect("query_items ran")
    }
    for params in [
        format!("parentId={PLAYLISTS_VIEW_ID}&recursive=true"),
        format!("parentId={PLAYLISTS_VIEW_ID}&recursive=true&includeItemTypes=Playlist"),
        format!("parentId={PLAYLISTS_FOLDER_ID}&includeItemTypes=Audio"),
    ] {
        let query = query_for(&params).await;
        assert_eq!(query.parent_id, Uuid::nil(), "{params}");
        assert!(query.recursive, "{params}");
        assert_eq!(
            query.include_item_types,
            [BaseItemKind::Playlist],
            "{params}"
        );
        assert!(query.linked_child_ancestor_ids.is_empty(), "{params}");
        assert!(query.apply_folder_display_options, "{params}");
    }
    // A view whose display parent is gone gets no folder policy of its own;
    // re-rooted, it takes the user root's, so the post-filter still runs.
    let stale = query_for(&format!(
        "parentId={STALE_PLAYLISTS_VIEW_ID}&recursive=true&includeItemTypes=Playlist"
    ))
    .await;
    assert_eq!(stale.parent_id, Uuid::nil());
    assert!(stale.apply_folder_display_options);
    let boxsets = query_for(&format!(
        "parentId={PLAYLISTS_FOLDER_ID}&recursive=true&includeItemTypes=BoxSet"
    ))
    .await;
    assert_eq!(boxsets.parent_id, Uuid::nil());
    assert_eq!(boxsets.include_item_types, [BaseItemKind::BoxSet]);
    assert_eq!(boxsets.linked_child_ancestor_ids, [PLAYLISTS_FOLDER_ID]);
}

/// Jellyfin 12's `GetItems` dispatch (v12.2 ItemsController.cs:326-340, 592;
/// `UserRootFolder.GetItemsInternal`):
/// - at the user root a filtered request runs recursively, while the bare
///   one stays the listing of the user's libraries;
/// - an item-typed browse of a library that did not choose `recursive` is
///   recursive, while an explicit `recursive=false` or an untyped browse
///   lists direct children;
/// - a `boxsets` library browsed untyped lists its box sets, recursively.
#[tokio::test]
async fn filtered_user_root_and_typed_library_browses_run_recursively() {
    use ferrofin_model::data::BaseItemKind;
    async fn query_for(params: &str) -> InternalItemsQuery {
        let library = OkLibrary {
            item_id: Uuid::from_u128(0x5A),
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: None,
            ..Default::default()
        };
        let seen = Arc::clone(&library.last_query);
        let response = create_router(ok_state_with(library))
            .oneshot(
                Request::builder()
                    .uri(format!("/Items?{params}"))
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{params}");
        seen.lock().unwrap().take().expect("query_items ran")
    }
    for (params, recursive) in [
        ("includeItemTypes=Movie&isFavorite=true".to_owned(), true),
        ("searchTerm=blade".to_owned(), true),
        ("genres=Drama&recursive=false".to_owned(), true),
        (String::new(), false),
        (
            format!("parentId={TV_LIBRARY_ID}&includeItemTypes=Episode"),
            true,
        ),
        (
            format!("parentId={TV_LIBRARY_ID}&includeItemTypes=Episode&recursive=false"),
            false,
        ),
        (format!("parentId={TV_LIBRARY_ID}"), false),
    ] {
        let query = query_for(&params).await;
        assert_eq!(query.recursive, recursive, "{params}");
        assert_eq!(query.user_root_children, !recursive, "{params}");
    }
    let boxsets = query_for(&format!("parentId={BOXSETS_LIBRARY_ID}")).await;
    assert_eq!(boxsets.parent_id, BOXSETS_LIBRARY_ID);
    assert_eq!(boxsets.include_item_types, [BaseItemKind::BoxSet]);
    assert!(boxsets.recursive);
    // Explicit ids keep their own path: no parent, no recursion.
    let ids = query_for(&format!("ids={ITEM_ID}&includeItemTypes=Movie")).await;
    assert_eq!(ids.item_ids, [ITEM_ID]);
    assert!(!ids.recursive && !ids.user_root_children);
    // A BoxSet browse of a TV library re-roots to the user root, where its
    // linked-ancestor constraint is a filter.
    let rerooted = query_for(&format!("parentId={TV_LIBRARY_ID}&includeItemTypes=BoxSet")).await;
    assert_eq!(rerooted.parent_id, Uuid::nil());
    assert_eq!(rerooted.linked_child_ancestor_ids, [TV_LIBRARY_ID]);
    assert!(rerooted.recursive);
    // A view is no library (`ICollectionFolder`), even one delegating to a
    // library: a typed browse of it stays a direct one.
    let view = query_for(&format!("parentId={TV_VIEW_ID}&includeItemTypes=Episode")).await;
    assert_eq!(view.parent_id, TV_LIBRARY_ID);
    assert!(!view.recursive);
}

/// Explicit ids without a `sortBy` answer in the order they were asked for:
/// `ItemsController.GetItems` hands them to `Folder.GetItems`, which reorders
/// the repository's page with `SortItemsByRequest` when `OrderBy` is empty and
/// there is more than one id (v12.2 Folder.cs:1065-1083). The legacy
/// `/Users/{userId}/Items` and `/Trailers` delegate to `GetItems`
/// (ItemsController.cs:725, TrailersController.cs:216-217). Only the page is
/// reordered: the start index and the total stay the repository's. Any
/// `sortBy`, `Default` included, keeps the repository's order, and so does a
/// search, which is ordered by relevance and `SortName`.
#[tokio::test]
async fn explicit_ids_answer_in_the_requested_order_unless_a_sort_is_asked() {
    let [alpha, bravo, charlie] = [0x852_0001, 0x852_0002, 0x852_0003].map(Uuid::from_u128);
    let names = |uri: String| async move {
        let library = OkLibrary {
            item_id: alpha,
            // The repository's default (`SortName`) order.
            rows: vec![
                item_entity(alpha, "Alpha", BaseItemKind::Movie),
                item_entity(bravo, "Bravo", BaseItemKind::Movie),
                item_entity(charlie, "Charlie", BaseItemKind::Movie),
            ],
            ..Default::default()
        };
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
        let body = json_body(response).await;
        assert_eq!(body["StartIndex"], 2, "{uri}: {body}");
        assert_eq!(body["TotalRecordCount"], PAGE_TOTAL, "{uri}: {body}");
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let ids = format!("ids={charlie},{alpha},{bravo}&startIndex=2");
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        for path in [
            "/Items".to_owned(),
            format!("/Users/{USER_ID}/Items"),
            "/Trailers".to_owned(),
        ] {
            assert_eq!(
                names(format!("{path}?{ids}")).await,
                ["Charlie", "Alpha", "Bravo"],
                "{path}: the requested order"
            );
            for kept in ["sortBy=SortName", "sortBy=Default", "searchTerm=a"] {
                assert_eq!(
                    names(format!("{path}?{ids}&{kept}")).await,
                    ["Alpha", "Bravo", "Charlie"],
                    "{path}?{kept}: the repository's order"
                );
            }
        }
    })
    .await
    .expect("the requests finish");
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
            ..Default::default()
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

/// v12.2 `ItemsController.GetItems` re-sorts a searched page by the search
/// providers' score, then `SortName` (ItemsController.cs:596-600), a stable
/// sort over what `folder.GetItems` returned in the caller's order. The
/// handler reproduces it by leading the caller's keys with `SortName`, which
/// `ApplyOrder`'s search branch puts right after the relevance rank. Without
/// a (non-blank) search the caller's keys pass through untouched.
#[tokio::test]
async fn get_items_orders_a_search_by_sort_name_before_the_callers_keys() {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    let order_by_for = |params: &'static str| async move {
        let library = OkLibrary {
            item_id: Uuid::from_u128(0x5B),
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: None,
            ..Default::default()
        };
        let seen = Arc::clone(&library.last_query);
        let response = create_router(ok_state_with(library))
            .oneshot(
                Request::builder()
                    .uri(format!("/Items?recursive=true&userId={USER_ID}&{params}"))
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{params}");
        seen.lock()
            .unwrap()
            .take()
            .expect("query_items ran")
            .order_by
    };
    assert_eq!(
        order_by_for("searchTerm=blade&sortBy=DateCreated&sortOrder=Descending").await,
        [
            (ItemSortBy::SortName, SortOrder::Ascending),
            (ItemSortBy::DateCreated, SortOrder::Descending)
        ]
    );
    assert_eq!(
        order_by_for("searchTerm=blade").await,
        [(ItemSortBy::SortName, SortOrder::Ascending)]
    );
    assert_eq!(
        order_by_for("sortBy=DateCreated&sortOrder=Descending").await,
        [(ItemSortBy::DateCreated, SortOrder::Descending)]
    );
    assert_eq!(
        order_by_for("searchTerm=%20&sortBy=DateCreated").await,
        [(ItemSortBy::DateCreated, SortOrder::Ascending)]
    );
}

/// Wholphin's genre grid asks each genre for one random item that has a
/// backdrop (`imageTypes=Backdrop`) and builds the card's image URL from it;
/// an item without a backdrop yields a null URL that crashes the whole grid
/// (`ConcurrentHashMap.put(genreId, null)`). The image type set must reach the
/// repository query; unknown tokens drop like the binder's.
#[tokio::test]
async fn get_items_forwards_the_image_types_filter() {
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5C),
        adopted_tree: false,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
        ..Default::default()
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

/// `GET /Items` forwards 12.1's `audioLanguages` / `subtitleLanguages` and
/// applies `GetItems`' media-stream rules (v12 ItemsController.cs:452-487);
/// the obsolete `GET /Users/{userId}/Items` passes `[]` for both lists
/// (ItemsController.cs:896-897), so it never filters by stream language.
/// The repeated, differently-cased `AUDIOLANGUAGES` key pins Ferrofin's own
/// leniency: the query normalizer joins repeated keys before the comma split,
/// so every piece binds, where upstream's multi-value arm
/// (CommaDelimitedCollectionModelBinder.cs:34-38) binds `["eng,ger", "fre"]`
/// (see `query_parse::parse_csv_strings`).
#[tokio::test]
async fn get_items_forwards_the_stream_language_filters_but_the_user_route_drops_them() {
    let filters = "recursive=true&audioLanguages=eng,ger&AUDIOLANGUAGES=fre\
                   &subtitleLanguages=spa&hasSubtitles=true";
    let run = |uri: String| async move {
        let library = OkLibrary {
            item_id: Uuid::from_u128(0x5E),
            adopted_tree: false,
            last_query: Arc::default(),
            deleted: Arc::default(),
            gate: None,
            ..Default::default()
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
        seen.lock().unwrap().take().expect("query_items ran")
    };

    let query = run(format!("/Items?{filters}")).await;
    assert_eq!(query.audio_languages, ["eng", "ger", "fre"]);
    assert_eq!(query.subtitle_languages, ["spa"]);
    // A subtitle language implies subtitles exist.
    assert_eq!(query.has_subtitles, None);
    assert!(query.include_owned_items);

    let query = run(format!("/Users/{USER_ID}/Items?{filters}")).await;
    assert!(query.audio_languages.is_empty());
    assert!(query.subtitle_languages.is_empty());
    assert_eq!(query.has_subtitles, Some(true));
    // `hasSubtitles` alone still reads media streams.
    assert!(query.include_owned_items);
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
    };
    let deleted = Arc::clone(&library.deleted);
    (
        ok_state_full(library, Arc::new(OkUsers), dto, auth),
        deleted,
    )
}

/// `LibraryController.DeleteItem`: `!item.CanDelete(user)` is `401
/// Unauthorized access` and nothing is deleted; allowed, the item is deleted
/// **with its files** (`DeleteOptions { DeleteFileLocation = true }`).
#[tokio::test]
async fn delete_item_asks_can_delete_and_deletes_the_files() {
    let item_id = Uuid::from_u128(0xAB51);
    let (state, deleted) = two_item_state(item_id, Arc::new(OkDto), alice());
    assert_eq!(
        delete(state, &format!("/Items/{item_id}")).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(*deleted.lock().unwrap(), vec![(item_id, true)]);

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
    assert_eq!(*deleted.lock().unwrap(), vec![(item_id, true)]);

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
    assert_eq!(*deleted.lock().unwrap(), vec![(item_id, true)]);
}

/// An API key has no user, so `CanDelete(user)` is not asked (upstream's
/// `user is not null && …`); a token with no user identity is refused by the
/// default policy before either delete handler can run.
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
        assert_eq!(*deleted.lock().unwrap(), vec![(item_id, true)]);
    }
    for (uri, expected) in [
        (format!("/Items/{item_id}"), StatusCode::FORBIDDEN),
        (format!("/Items?ids={item_id}"), StatusCode::FORBIDDEN),
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
            ..Default::default()
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
        assert_eq!(*deleted.lock().unwrap(), vec![(item_id, true)]);
    }
}

#[tokio::test]
async fn default_season_view_preserves_actual_parent_for_live_aired_selection_and_ids_bypass_it() {
    for recursive in [false, true] {
        for explicit in [false, true] {
            let library = OkLibrary {
                item_id: ITEM_ID,
                adopted_tree: false,
                last_query: Arc::default(),
                deleted: Arc::default(),
                gate: None,
                ..Default::default()
            };
            let seen = library.last_query.clone();
            let router = create_router(ok_state_with(library));
            let path = format!(
                "/Items?parentId={SEASON_VIEW_ID}&recursive={recursive}{}",
                if explicit {
                    format!("&ids={ITEM_ID}")
                } else {
                    String::new()
                }
            );
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("X-Emby-Token", "valid")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let query = seen.lock().unwrap().clone().expect("repository query");
            assert_eq!(
                query.parent_id,
                if explicit {
                    Uuid::nil()
                } else {
                    SEASON_BROWSE_ID
                }
            );
            assert_eq!(
                query.parent_type,
                if explicit {
                    None
                } else {
                    Some(BaseItemKind::Season)
                }
            );
            assert_eq!(query.apply_folder_display_options, !explicit);
            assert!(
                !query.folder_query_recursive,
                "default delegation keeps the Season override instead of invoking Folder.QueryRecursive"
            );
        }
    }
}

/// Plain nullable string query parameters use ASP.NET ConvertEmptyStringToNull.
/// This is an HTTP boundary, not permission to trim nonblank internal filters.
#[tokio::test]
async fn items_http_name_boundaries_bind_blank_to_null_and_preserve_nonblank() {
    for path in ["/Items".to_owned(), format!("/Users/{USER_ID}/Items")] {
        for (encoded, expected) in [
            ("", None),
            ("%20", None),
            ("%09%0A%0D", None),
            ("%C2%A0%E2%80%83", None),
            ("%20A%20", Some(" A ")),
            ("%25", Some("%")),
            ("_", Some("_")),
        ] {
            let library = OkLibrary {
                item_id: Uuid::from_u128(0x5A),
                adopted_tree: false,
                last_query: Arc::default(),
                deleted: Arc::default(),
                gate: None,
                ..Default::default()
            };
            let seen = Arc::clone(&library.last_query);
            let uri = format!(
                "{path}?recursive=true&nameStartsWith={encoded}&nameStartsWithOrGreater={encoded}&nameLessThan={encoded}"
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
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let query = seen.lock().unwrap().take().expect("query_items ran");
            assert_eq!(query.name_starts_with.as_deref(), expected, "{uri}");
            assert_eq!(
                query.name_starts_with_or_greater.as_deref(),
                expected,
                "{uri}"
            );
            assert_eq!(query.name_less_than.as_deref(), expected, "{uri}");
        }
    }
}

async fn capture_items_filter_query(uri: &str) -> (StatusCode, Option<InternalItemsQuery>) {
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5A),
        adopted_tree: false,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
        ..Default::default()
    };
    let seen = Arc::clone(&library.last_query);
    let response = create_router(ok_state_with(library))
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("X-Emby-Token", "valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let captured = seen.lock().unwrap().take();
    (response.status(), captured)
}

/// Both controller routes forward every exposed GetResult presence flag,
/// including nullable boolean binding rather than silently discarding it.
#[tokio::test]
async fn items_get_result_presence_flags_reach_both_routes() {
    for path in ["/Items".to_owned(), format!("/Users/{USER_ID}/Items")] {
        for (encoded, expected) in [
            ("true", Some(true)),
            ("false", Some(false)),
            ("%20TrUe%20", Some(true)),
            ("%09FaLsE%0A", Some(false)),
            ("", None),
            ("%20", None),
            ("%C2%A0%E2%80%83", None),
        ] {
            let uri = format!(
                "{path}?recursive=true&hasOverview={encoded}&hasImdbId={encoded}&hasTmdbId={encoded}&hasTvdbId={encoded}&hasParentalRating={encoded}"
            );
            let (status, query) = capture_items_filter_query(&uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            let query = query.expect("query_items ran");
            assert_eq!(query.has_overview, expected, "{uri}");
            assert_eq!(query.has_imdb_id, expected, "{uri}");
            assert_eq!(query.has_tmdb_id, expected, "{uri}");
            assert_eq!(query.has_tvdb_id, expected, "{uri}");
            assert_eq!(query.has_parental_rating, expected, "{uri}");
        }
    }
}

/// Inclusive premiere dates are normalized to UTC without dropping 100ns
/// precision; empty dates and empty adjacency remain absent on both routes.
#[tokio::test]
async fn items_get_result_dates_and_adjacency_reach_both_routes() {
    let adjacent = Uuid::from_u128(0x5678);
    for path in ["/Items".to_owned(), format!("/Users/{USER_ID}/Items")] {
        for (encoded, expected) in [
            (
                "2026-01-02T03:04:05.0000001%2B02:00",
                Some("2026-01-02T01:04:05.0000001Z"),
            ),
            (
                "2026-01-02T03:04:05.0000001Z",
                Some("2026-01-02T03:04:05.0000001Z"),
            ),
            ("", None),
            ("%20", None),
        ] {
            let uri = format!(
                "{path}?recursive=true&minPremiereDate={encoded}&maxPremiereDate={encoded}&adjacentTo={adjacent}"
            );
            let (status, query) = capture_items_filter_query(&uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            let query = query.expect("query_items ran");
            let expected =
                expected.map(|raw| raw.parse::<chrono::DateTime<chrono::Utc>>().unwrap());
            assert_eq!(query.min_premiere_date, expected, "{uri}");
            assert_eq!(query.max_premiere_date, expected, "{uri}");
            assert_eq!(query.adjacent_to, Some(adjacent), "{uri}");
        }
        for (encoded, expected) in [
            ("", None),
            ("%20", None),
            ("00000000-0000-0000-0000-000000000000", Some(Uuid::nil())),
        ] {
            let uri = format!("{path}?recursive=true&adjacentTo={encoded}");
            let (status, query) = capture_items_filter_query(&uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(
                query.expect("query_items ran").adjacent_to,
                expected,
                "{uri}"
            );
        }
    }
}

/// Invalid exposed filters fail at binding instead of returning unfiltered data.
#[tokio::test]
async fn items_get_result_invalid_filters_fail_before_querying() {
    for path in ["/Items".to_owned(), format!("/Users/{USER_ID}/Items")] {
        for parameter in [
            "hasOverview=1",
            "hasImdbId=yes",
            "hasTmdbId=null",
            "hasTvdbId=no",
            "hasParentalRating=maybe",
            "minPremiereDate=invalid",
            "maxPremiereDate=2026-02-30",
            "adjacentTo=not-a-guid",
        ] {
            let uri = format!("{path}?recursive=true&{parameter}");
            let (status, query) = capture_items_filter_query(&uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert!(query.is_none(), "{uri} must fail before querying");
        }
    }
}

#[cfg(unix)]
#[derive(serde::Deserialize)]
struct ItemsDateOracleRow {
    timezone: String,
    raw: String,
    utc: Option<String>,
}

/// The UTC instant `row` binds to on this host. Dublin's spring gap and
/// autumn fold resolve through the zone's standard offset (.NET's
/// `NoThrowOnInvalidTime`; Ferrofin's `mktime` with `tm_isdst = 0`), and the
/// host's tzdata decides which offset that is: the main format makes Irish
/// Standard Time (+01:00) standard with a negative winter DST, the rearguard
/// format (Ubuntu's, so GitHub's runners) makes GMT standard. The oracle ran
/// on main-format data; on a rearguard host .NET reads the same TZif as
/// Ferrofin and answers an hour later.
#[cfg(unix)]
fn host_utc(row: &ItemsDateOracleRow) -> Option<chrono::DateTime<chrono::Utc>> {
    let utc = row.utc.as_deref()?.parse().unwrap();
    let transition = row.timezone == "Europe/Dublin"
        && ["2026-03-29T01:30:00", "2026-10-25T01:30:00"]
            .iter()
            .any(|wall| row.raw.starts_with(wall));
    if transition && winter_is_standard_time() {
        return Some(utc + chrono::Duration::hours(1));
    }
    Some(utc)
}

/// Whether the process's zone (`TZ`) reports mid-January as standard time:
/// false for a main-format Europe/Dublin, whose winter GMT is the DST.
#[cfg(unix)]
fn winter_is_standard_time() -> bool {
    let january: libc::time_t = 1_768_478_400; // 2026-01-15T12:00:00Z
    // SAFETY: localtime_r reads the zone the child process started with
    // (`TZ`), writes one zero-initialized tm and returns null only when the
    // time is unrepresentable.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        assert!(!libc::localtime_r(&raw const january, &raw mut tm).is_null());
        tm.tm_isdst == 0
    }
}

#[cfg(unix)]
fn items_date_oracle_rows() -> Vec<ItemsDateOracleRow> {
    if let Ok(path) = std::env::var("FERROFIN_ITEMS_DATE_ORACLE") {
        return serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    }
    // Actual BCL10 oracle results, with stable named-zone transition dates.
    // A root-only validation may supply the full measured160+row JSON using
    // FERROFIN_ITEMS_DATE_ORACLE; neither path mutates the test process TZ.
    let cases = [
        ("UTC", "2026-01-02", "2026-01-02T00:00:00Z"),
        ("UTC", "1969-12-31T23:59:59", "1969-12-31T23:59:59Z"),
        (
            "America/New_York",
            "2026-01-02T03:04:05.0000001",
            "2026-01-02T08:04:05.0000001Z",
        ),
        (
            "America/New_York",
            "2026-07-02T03:04:05.0000001",
            "2026-07-02T07:04:05.0000001Z",
        ),
        ("America/New_York", "2026-01-02", "2026-01-02T05:00:00Z"),
        (
            "America/New_York",
            "2026-03-08T02:30:00.0000001",
            "2026-03-08T07:30:00.0000001Z",
        ),
        (
            "America/New_York",
            "2026-11-01T01:30:00.0000001",
            "2026-11-01T06:30:00.0000001Z",
        ),
        (
            "America/New_York",
            "0001-01-01T00:00:00",
            "0001-01-01T04:57:00Z",
        ),
        (
            "America/New_York",
            "9999-12-31T23:59:59.9999999",
            "9999-12-31T23:59:59.9999999Z",
        ),
        (
            "Europe/Dublin",
            "2026-03-29T01:30:00.0000001",
            "2026-03-29T00:30:00.0000001Z",
        ),
        (
            "Europe/Dublin",
            "2026-10-25T01:30:00.0000001",
            "2026-10-25T00:30:00.0000001Z",
        ),
        (
            "Europe/Dublin",
            "0001-01-01T00:00:00",
            "0001-01-01T00:25:00Z",
        ),
        (
            "Australia/Lord_Howe",
            "2026-04-05T01:45:00.0000001",
            "2026-04-04T15:15:00.0000001Z",
        ),
        (
            "Australia/Lord_Howe",
            "2026-10-04T02:15:00.0000001",
            "2026-10-03T15:45:00.0000001Z",
        ),
        (
            "Australia/Lord_Howe",
            "0001-01-01T00:00:00",
            "0001-01-01T00:00:00Z",
        ),
    ];
    cases
        .into_iter()
        .map(|(zone, raw, utc)| ItemsDateOracleRow {
            timezone: zone.to_owned(),
            raw: raw.to_owned(),
            utc: Some(utc.to_owned()),
        })
        .collect()
}

#[cfg(unix)]
async fn capture_items_date_socket_query(uri: &str) -> (StatusCode, Option<InternalItemsQuery>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let library = OkLibrary {
        item_id: Uuid::from_u128(0x5A),
        adopted_tree: false,
        last_query: Arc::default(),
        deleted: Arc::default(),
        gate: None,
        ..Default::default()
    };
    let seen = Arc::clone(&library.last_query);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = create_router(ok_state_with(library));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("GET {uri} HTTP/1.1\r\nHost: {address}\r\nX-Emby-Token: valid\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        socket.read_to_end(&mut bytes),
    )
    .await;
    server.abort();
    let _ = server.await;
    read.expect("owned HTTP server responds").unwrap();
    let response = String::from_utf8(bytes).unwrap();
    let code = response
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let captured = seen.lock().unwrap().take();
    (StatusCode::from_u16(code).unwrap(), captured)
}

/// The actual modern and legacy Items routes bind local dates in owned child
/// processes so concurrent tests never change their environment or timezone.
#[cfg(unix)]
#[tokio::test]
async fn items_get_result_local_dates_match_dotnet_child_process() {
    let rows = items_date_oracle_rows();
    let Ok(zone) = std::env::var("FERROFIN_ITEMS_DATE_TEST_ZONE") else {
        let zones = rows
            .iter()
            .map(|row| &row.timezone)
            .collect::<std::collections::BTreeSet<_>>();
        for zone in zones {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "items_get_result_local_dates_match_dotnet_child_process",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("TZ", zone)
                .env("FERROFIN_ITEMS_DATE_TEST_ZONE", zone)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{zone}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    for row in rows.iter().filter(|row| row.timezone == zone) {
        let encoded = form_urlencoded::byte_serialize(row.raw.as_bytes()).collect::<String>();
        for path in ["/Items".to_owned(), format!("/Users/{USER_ID}/Items")] {
            let uri = format!(
                "{path}?recursive=true&minPremiereDate={encoded}&maxPremiereDate={encoded}"
            );
            let (status, query) = capture_items_date_socket_query(&uri).await;
            if let Some(expected) = host_utc(row) {
                assert_eq!(status, StatusCode::OK, "{zone}: {uri}");
                let query = query.expect("query_items ran");
                let expected = Some(expected);
                assert_eq!(query.min_premiere_date, expected, "min {zone}: {uri}");
                assert_eq!(query.max_premiere_date, expected, "max {zone}: {uri}");
            } else {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{zone}: {uri}");
                assert!(query.is_none(), "{zone}: {uri} must fail before querying");
            }
        }
    }
}

/// [`OkDto`], with the DTOs [`DtoService::get_linked_children_dtos`] built
/// marked in `Overview`, so a test can see which projection a listing used.
struct LinkedChildrenDto;

#[async_trait]
impl DtoService for LinkedChildrenDto {
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
    async fn get_linked_children_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|item| BaseItemDto {
                overview: Some("linked children".to_owned()),
                ..entity_to_dto(item)
            })
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
}

/// A playlist's or box set's own listing is its linked children
/// (`Folder.GetItemsInternal` → `GetChildren` → `GetLinkedChildren(user)`,
/// v12.2 Folder.cs), which upstream resolves with every field loaded, so the
/// members take the linked-children projection; a recursive listing is the
/// repository query and stays a plain page.
#[tokio::test]
async fn a_playlist_or_box_set_listing_projects_its_linked_children() {
    for (params, linked) in [
        (format!("parentId={PLAYLIST_ID}"), true),
        (format!("parentId={BOX_SET_ID}"), true),
        (format!("parentId={BOX_SET_ID}&recursive=true"), false),
        // Named ids clear the parent: a plain lookup, not a listing.
        (format!("parentId={BOX_SET_ID}&ids={ITEM_ID}"), false),
        (format!("parentId={TV_LIBRARY_ID}"), false),
    ] {
        let state = ok_state_full(
            OkLibrary {
                item_id: ITEM_ID,
                ..Default::default()
            },
            Arc::new(OkUsers),
            Arc::new(LinkedChildrenDto),
            (Arc::new(OkAuthContext), Arc::new(OkAuthService)),
        );
        let response = create_router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("/Items?userId={USER_ID}&{params}"))
                    .header("X-Emby-Token", "valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{params}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            page["Items"][0]["Overview"].as_str() == Some("linked children"),
            linked,
            "{params}: {page}"
        );
    }
}
