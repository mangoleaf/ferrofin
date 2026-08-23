//! Fake `ferrofin-traits` manager impls for testing `ferrofin-api` in isolation.
//!
//! These are minimal **test doubles**, not real implementations — `ferrofin-api`
//! must never dev-depend on `ferrofin-core`. Only the two authorization traits
//! carry meaningful behaviour (they are the ones the router's auth-context
//! middleware invokes); every other manager method is `unimplemented!()`, since
//! the INFRA-level tests exercise only routing, health, and auth, never the
//! domain managers.
//!
//! Gated behind the `test-util` feature so it is compiled for this crate's tests
//! (unit and integration) but never shipped in a production build.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use ferrofin_db::entities::playback::TrickplayInfoEntity;
use ferrofin_db::entities::security::{DeviceEntity, DeviceOptionsEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::activity::ActivityLogEntry;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::{
    MetadataOptions, MetadataPluginSummary, ServerConfiguration, UserConfiguration,
};
use ferrofin_model::data::CollectionType;
use ferrofin_model::devices::DeviceInfo;
use ferrofin_model::dto::{
    BaseItemDto, ClientCapabilitiesDto, DeviceInfoDto, ItemCounts, MediaSourceInfo, NameIdPair,
    SessionInfoDto, UpdateUserItemDataDto, UserDto, UserItemDataDto,
};
use ferrofin_model::entities::ImageType;
use ferrofin_model::entities_media::PlaylistUserPermissions;
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::entities_media::{ParentalRating, ParentalRatingScore};
use ferrofin_model::globalization::{CountryInfo, CultureDto, LocalizationOption};
use ferrofin_model::io::FileSystemEntryInfo;
use ferrofin_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use ferrofin_model::media_info::LiveStreamRequest;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_model::playlists::{
    PlaylistCreationRequest, PlaylistCreationResult, PlaylistUpdateRequest,
    PlaylistUserUpdateRequest,
};
use ferrofin_model::providers::{
    ExternalIdInfo, ImageProviderInfo, RemoteImageInfo, RemoteImageQuery,
};
use ferrofin_model::providers::{LyricProviderInfo, RemoteSubtitleInfo, SubtitleProviderInfo};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_model::quick_connect::QuickConnectResult;
use ferrofin_model::search::{SearchHint, SearchQuery};
use ferrofin_model::security::AuthenticationInfo;
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use ferrofin_model::system::{PublicSystemInfo, SystemInfo, SystemStorageInfo};
use ferrofin_model::tasks::{TaskInfo, TaskTriggerInfo};
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::activity::{ActivityLogQuery, ActivityManager};
use ferrofin_traits::collections::{CollectionCreationOptions, CollectionManager, PlaylistManager};
use ferrofin_traits::configuration::{DisplayPreferencesManager, ServerConfigurationManager};
use ferrofin_traits::devices::{DeviceManager, DeviceQuery};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::ClientEventLogger;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::{
    LibraryManager, LibraryMonitor, MediaSourceManager, MusicManager, SearchManager, SearchResult,
    SimilarItemsManager, SimilarItemsRecommendation, UserDataManager, UserManager, UserViewManager,
    VirtualFolderManager,
};
use ferrofin_traits::localization::LocalizationManager;
use ferrofin_traits::media_encoding::SubtitleEncoder;
use ferrofin_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::{
    AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::persistence::ItemWithCounts;
use ferrofin_traits::providers::{
    ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use ferrofin_traits::security::{ApiKeyManager, QuickConnect};
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use ferrofin_traits::stubs::LyricManager;
use ferrofin_traits::subtitles::{SubtitleManager, SubtitleResponse, SubtitleSearchRequest};
use ferrofin_traits::system::{ServerApplicationHost, ServerApplicationPaths, SystemManager};
use ferrofin_traits::tasks::TaskManager;
use ferrofin_traits::trickplay::TrickplayManager;
use ferrofin_traits::tv::{NextUpQuery, TvSeriesManager};
use uuid::Uuid;

use crate::state::AppState;

/// Builds an [`AppState`] whose every manager is a fake test double.
///
/// The authorization traits behave sensibly (anonymous context / unauthorized);
/// all other managers panic if called, which is exactly what the routing-level
/// tests want — they never reach a real domain manager.
#[must_use]
pub fn fake_state() -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(FakeAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Like [`fake_state`] but with an [`AuthedAuthService`] that always
/// authenticates, so `RequireAuth`-guarded routes reach their handler.
#[must_use]
pub fn authed_fake_state() -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(AuthedAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Like [`authed_fake_state`] but authenticated as an **API key**, which
/// satisfies Jellyfin's `RequiresElevation` policy without a user/policy
/// lookup — so [`RequireAdmin`](crate::auth::RequireAdmin)-guarded routes
/// reach their handler.
///
/// Use this for tests of an admin-only controller's behaviour. The gate itself
/// is pinned end to end by `apps/ferrofin-server/tests/elevation.rs`, against a
/// real non-administrator token.
#[must_use]
pub fn elevated_fake_state() -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(ApiKeyAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Builds a minimal [`BaseItemEntity`] with the given id, name, and type key.
///
/// Every other column is a neutral zero/`None`, so integration tests that only
/// need an item to *exist* (or to carry a `Path`/`Width`) don't repeat the full
/// ~80-field literal. Set the fields a test cares about on the returned value.
#[must_use]
pub fn minimal_base_item(id: Uuid, name: &str, type_key: &str) -> BaseItemEntity {
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
        type_: type_key.to_owned(),
        unrated_type: None,
        width: None,
    }
}

/// A fake [`TvSeriesManager`]; every method is unused by INFRA-level tests.
pub struct FakeTvSeries;

#[async_trait]
impl TvSeriesManager for FakeTvSeries {
    async fn get_next_up(
        &self,
        _query: &NextUpQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`AuthorizationContext`] that always resolves to an anonymous,
/// unauthenticated context (never rejecting), mirroring a request with no token.
pub struct FakeAuthContext;

#[async_trait]
impl AuthorizationContext for FakeAuthContext {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo::default())
    }
}

/// A fake [`AuthService`] that always rejects (no valid credentials), so
/// `RequireAuth`-guarded routes return `401` in tests.
pub struct FakeAuthService;

#[async_trait]
impl AuthService for FakeAuthService {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Err(ServiceError::unauthorized("no credentials"))
    }
}

/// A fake [`AuthService`] that always authenticates, so `RequireAuth`-guarded
/// routes reach their handler in tests exercising handler logic (not auth).
pub struct AuthedAuthService;

#[async_trait]
impl AuthService for AuthedAuthService {
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

/// A fake [`AuthService`] that authenticates as an **API key** (no user).
/// API keys satisfy the admin-gated plugin routes (Jellyfin's
/// `RequiresElevation` treats them as elevated), so this exercises handler
/// logic behind the gate without a user/policy lookup.
pub struct ApiKeyAuthService;

#[async_trait]
impl AuthService for ApiKeyAuthService {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            is_authenticated: true,
            is_api_key: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A fake [`LibraryManager`]; every method is unused by INFRA-level tests.
pub struct FakeLibrary;

#[async_trait]
impl LibraryManager for FakeLibrary {
    async fn get_item_by_id(&self, _id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_list(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_item(&self, _id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_people_names(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_counts(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("fake")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`UserManager`]; every method is unused by INFRA-level tests.
pub struct FakeUsers;

#[async_trait]
impl UserManager for FakeUsers {
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("fake")
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_by_id(&self, _id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        // Benign default: no user. Callers that reload a user for enrichment
        // (e.g. the auth-result User DTO) fall back gracefully on `None`.
        Ok(None)
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn change_password(
        &self,
        _user_id: Uuid,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_user(
        &self,
        _username: &str,
        _password: &str,
        _remote_endpoint: &str,
        _is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_dto(
        &self,
        _user: &UserEntity,
        _server_id: Option<String>,
    ) -> Result<UserDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`UserViewManager`]; every method is unused by INFRA-level tests.
pub struct FakeUserViews;

#[async_trait]
impl UserViewManager for FakeUserViews {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_media_folders(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_latest_items(
        &self,
        _query: &ferrofin_traits::options::LatestItemsQuery,
        _options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`UserDataManager`]; every method is unused by INFRA-level tests.
pub struct FakeUserData;

#[async_trait]
impl UserDataManager for FakeUserData {
    async fn save_user_data(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_data_dto(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_user_data_batch(
        &self,
        _item_ids: &[Uuid],
        _user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_play_state(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        unimplemented!("fake")
    }
    async fn mark_played(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn mark_unplayed(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn reset_playback_stream_selections(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`MediaSourceManager`]; every method is unused by INFRA-level tests.
pub struct FakeMediaSources;

#[async_trait]
impl MediaSourceManager for FakeMediaSources {
    async fn get_media_streams(&self, _item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaAttachment>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_playback_media_sources(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        _enable_path_substitution: bool,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn open_live_stream(
        &self,
        _request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
        unimplemented!("fake")
    }
    async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn refresh_media_streams(&self, _item_id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// The deterministic access token [`FakeSessions`] mints on authentication, so
/// handler tests can assert the token is echoed in the `AuthenticationResult`.
pub const FAKE_ACCESS_TOKEN: &str = "fake-access-token";

/// A fake [`SessionManager`]; every method is unused by INFRA-level tests.
pub struct FakeSessions;

#[async_trait]
impl SessionManager for FakeSessions {
    async fn log_session_activity(
        &self,
        _app_name: &str,
        _app_version: &str,
        _device_id: &str,
        _device_name: &str,
        _remote_endpoint: &str,
        _user: &UserEntity,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_device_name(
        &self,
        _session_id: &str,
        _reported_device_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_start(&self, _info: &PlaybackStartInfo) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_progress(
        &self,
        _info: &PlaybackProgressInfo,
        _is_automated: bool,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_stopped(&self, _info: &PlaybackStopInfo) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_session_ended(&self, _session_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_general_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &GeneralCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &MessageCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_play_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &PlayRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_playstate_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &PlaystateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_admin_sessions(
        &self,
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_user_sessions(
        &self,
        _user_ids: &[Uuid],
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        // The played/favorite/rating/user-data handlers push `UserDataChanged`
        // here best-effort; delivery is covered by ferrofin-core's tests.
        Ok(())
    }
    async fn send_message_to_user_device_sessions(
        &self,
        _device_id: &str,
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn add_additional_user(
        &self,
        _session_id: &str,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_additional_user(
        &self,
        _session_id: &str,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_now_viewing_item(
        &self,
        _session_id: &str,
        _item_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_new_session(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        Ok(AuthenticationResultData {
            session: SessionInfoDto::default(),
            access_token: FAKE_ACCESS_TOKEN.into(),
        })
    }
    async fn authenticate_direct(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        Ok(AuthenticationResultData {
            session: SessionInfoDto::default(),
            access_token: FAKE_ACCESS_TOKEN.into(),
        })
    }
    async fn report_capabilities(
        &self,
        _session_id: &str,
        _capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_transcoding_info(
        &self,
        _device_id: &str,
        _info: &TranscodingInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn clear_transcoding_info(&self, _device_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_sessions(
        &self,
        _user_id: Uuid,
        _device_id: Option<&str>,
        _active_within_seconds: Option<i32>,
        _controllable_user_to_check: Option<Uuid>,
        _is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_session_by_authentication_token(
        &self,
        _token: &str,
        _device_id: &str,
        _remote_endpoint: &str,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn logout(&self, _access_token: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn logout_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn revoke_user_tokens(
        &self,
        _user_id: Uuid,
        _current_access_token: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn close_live_stream_if_needed(
        &self,
        _live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`SystemManager`]. The info getters return a default value so the
/// now-real `/System/Info` and `/System/Info/Public` handlers resolve (the
/// contract probe expects them to route, not `404`); the lifecycle/storage
/// methods stay `unimplemented!` (never exercised by these tests).
pub struct FakeSystem;

#[async_trait]
impl SystemManager for FakeSystem {
    async fn get_system_info(&self, _request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        Ok(SystemInfo::default())
    }
    async fn get_public_system_info(
        &self,
        _request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        Ok(PublicSystemInfo::default())
    }
    async fn restart(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn shutdown(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_system_storage_info(&self) -> Result<SystemStorageInfo, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`ServerApplicationHost`]; getters return neutral values, URL builders
/// are unused by INFRA-level tests.
pub struct FakeAppHost;

#[async_trait]
impl ServerApplicationHost for FakeAppHost {
    fn core_startup_has_completed(&self) -> bool {
        true
    }
    fn http_port(&self) -> u16 {
        8096
    }
    fn https_port(&self) -> u16 {
        8920
    }
    fn listen_with_https(&self) -> bool {
        false
    }
    fn friendly_name(&self) -> String {
        "ferrofin-test".to_owned()
    }
    async fn get_smart_api_url(&self, _request: &RequestContext) -> Result<String, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_local_api_url(
        &self,
        _hostname: &str,
        _scheme: Option<&str>,
        _port: Option<u16>,
    ) -> Result<String, ServiceError> {
        unimplemented!("fake")
    }
    fn expand_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
    fn reverse_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
}

/// A fake [`ServerApplicationPaths`]; every accessor returns an empty path.
pub struct FakePaths;

impl ServerApplicationPaths for FakePaths {
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
        // A process-unique temp root so path-backed handlers (e.g. Backup) have a
        // real, writable directory to operate in during tests.
        std::env::temp_dir()
            .join("ferrofin-api-test-data")
            .to_string_lossy()
            .into_owned()
    }
    fn web_path(&self) -> String {
        String::new()
    }
    fn data_path(&self) -> String {
        self.program_data_path()
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

/// A fake [`ServerConfigurationManager`]; `application_paths` yields [`FakePaths`],
/// the rest are unused by INFRA-level tests.
pub struct FakeConfig;

#[async_trait]
impl ServerConfigurationManager for FakeConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(FakePaths)
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        // Default config has `is_startup_wizard_completed = false`, so the
        // `FirstTimeSetupOrAuth` extractor takes its anonymous first-time-setup
        // path in tests (matching a fresh install).
        Ok(Arc::new(ServerConfiguration::default()))
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_branding(&self, _branding: &BrandingOptions) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`ProviderManager`]; every method is unused by INFRA-level tests.
pub struct FakeProviders;

#[async_trait]
impl ProviderManager for FakeProviders {
    async fn queue_refresh(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        unimplemented!("fake")
    }
    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_metadata_options(&self, _item_id: Uuid) -> Result<MetadataOptions, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`MusicManager`]; every method is unused by INFRA-level tests.
pub struct FakeMusic;

#[async_trait]
impl MusicManager for FakeMusic {
    async fn get_instant_mix_from_item(
        &self,
        _item_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_instant_mix_from_artist(
        &self,
        _artist_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_instant_mix_from_genres(
        &self,
        _genres: &[String],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`SimilarItemsManager`]; every method is unused by INFRA-level tests.
pub struct FakeSimilarItems;

#[async_trait]
impl SimilarItemsManager for FakeSimilarItems {
    async fn get_similar_items(
        &self,
        _item_id: Uuid,
        _exclude_artist_ids: &[Uuid],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
        _limit: Option<i32>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_movie_recommendations(
        &self,
        _user_id: Option<Uuid>,
        _parent_id: Uuid,
        _category_limit: i32,
        _item_limit: i32,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`SearchManager`]; every method is unused by INFRA-level tests.
pub struct FakeSearch;

#[async_trait]
impl SearchManager for FakeSearch {
    async fn get_search_hints(
        &self,
        _query: &SearchQuery,
    ) -> Result<QueryResult<SearchHint>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_search_results(
        &self,
        _query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`DtoService`]; every method is unused by INFRA-level tests.
pub struct FakeDto;

#[async_trait]
impl DtoService for FakeDto {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_base_item_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_base_item_dtos(
        &self,
        _items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`QuickConnect`]; every method is unused by INFRA-level tests.
pub struct FakeQuickConnect;

#[async_trait]
impl QuickConnect for FakeQuickConnect {
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        unimplemented!("fake")
    }
    async fn try_connect(
        &self,
        _authorization_info: &AuthorizationInfo,
    ) -> Result<QuickConnectResult, ServiceError> {
        unimplemented!("fake")
    }
    async fn check_request_status(
        &self,
        _secret: &str,
    ) -> Result<QuickConnectResult, ServiceError> {
        unimplemented!("fake")
    }
    async fn authorize_request(&self, _user_id: Uuid, _code: &str) -> Result<bool, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_authorized_request(&self, _secret: &str) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`PlaylistManager`]; every method is unused by INFRA-level tests.
pub struct FakePlaylists;

#[async_trait]
impl PlaylistManager for FakePlaylists {
    async fn get_playlist_access(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<ferrofin_traits::collections::PlaylistAccess, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_playlist_for_user(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<BaseItemEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_playlist(
        &self,
        _request: &PlaylistCreationRequest,
    ) -> Result<PlaylistCreationResult, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_playlist(&self, _request: &PlaylistUpdateRequest) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_playlists(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_playlist_items(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn add_user_to_shares(
        &self,
        _request: &PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_user_from_shares(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
        _share: &PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_playlist_shares(
        &self,
        _playlist_id: Uuid,
    ) -> Result<Vec<PlaylistUserPermissions>, ServiceError> {
        Ok(Vec::new())
    }
    async fn add_item_to_playlist(
        &self,
        _playlist_id: Uuid,
        _item_ids: &[Uuid],
        _position: Option<i32>,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_item_from_playlist(
        &self,
        _playlist_id: &str,
        _entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn move_item(
        &self,
        _playlist_id: &str,
        _entry_id: &str,
        _new_index: i32,
        _calling_user_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_playlists(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`CollectionManager`]; every method is unused by INFRA-level tests.
pub struct FakeCollections;

#[async_trait]
impl CollectionManager for FakeCollections {
    async fn create_collection(
        &self,
        _options: &CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn add_to_collection(
        &self,
        _collection_id: Uuid,
        _item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_from_collection(
        &self,
        _collection_id: Uuid,
        _item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_collections_containing_item(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_collections_folder(
        &self,
        _create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`SubtitleManager`]; every method is unused by INFRA-level tests.
pub struct FakeSubtitles;

#[async_trait]
impl SubtitleManager for FakeSubtitles {
    async fn search_subtitles(
        &self,
        _request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn download_subtitles(
        &self,
        _item_id: Uuid,
        _subtitle_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn upload_subtitle(
        &self,
        _item_id: Uuid,
        _response: &SubtitleResponse,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_remote_subtitles(&self, _id: &str) -> Result<SubtitleResponse, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_subtitles(&self, _item_id: Uuid, _index: i32) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<SubtitleProviderInfo>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`LyricManager`]; every method is unused by INFRA-level tests.
pub struct FakeLyrics;

#[async_trait]
impl LyricManager for FakeLyrics {
    async fn get_lyrics(&self, _item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn search_lyrics(&self, _item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn download_lyrics(
        &self,
        _item_id: Uuid,
        _lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_remote_lyrics(&self, _lyric_id: &str) -> Result<Option<LyricDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn save_lyric(
        &self,
        _item_id: Uuid,
        _format: &str,
        _lyrics: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_lyrics(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<LyricProviderInfo>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`MediaSegmentManager`]; every method is unused by INFRA-level tests.
pub struct FakeMediaSegments;

#[async_trait]
impl MediaSegmentManager for FakeMediaSegments {
    async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_segment(
        &self,
        _segment: &MediaSegmentDto,
        _segment_provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_segment(&self, _segment_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_segments(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_segments(
        &self,
        _item_id: Uuid,
        _type_filter: Option<&[MediaSegmentType]>,
        _filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        // Consulted by every PlaybackInfo call (the `HasSegments` stamp).
        Ok(false)
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_provider_segments(
        &self,
        _item_id: Uuid,
        _provider_id: &str,
        _type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`TrickplayManager`]; every method is unused by INFRA-level tests.
pub struct FakeTrickplay;

#[async_trait]
impl TrickplayManager for FakeTrickplay {
    async fn refresh_trickplay_data(
        &self,
        _item_id: Uuid,
        _replace: bool,
        _library_options: &ferrofin_model::configuration::LibraryOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_trickplay_resolutions(
        &self,
        _item_id: Uuid,
    ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_trickplay_items(
        &self,
        _limit: i32,
        _offset: i32,
    ) -> Result<Vec<TrickplayInfoEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn save_trickplay_info(&self, _info: &TrickplayInfoEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_trickplay_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_trickplay_manifest(
        &self,
        _item_id: Uuid,
    ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_hls_playlist(
        &self,
        _item_id: Uuid,
        _width: i32,
        _api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_trickplay_tile_path(
        &self,
        _item_id: Uuid,
        _width: i32,
        _index: i32,
    ) -> Result<Option<String>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`DeviceManager`]; every method is unused by INFRA-level tests.
pub struct FakeDevices;

#[async_trait]
impl DeviceManager for FakeDevices {
    async fn create_device(&self, _device: &DeviceEntity) -> Result<DeviceEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn save_capabilities(
        &self,
        _device_id: &str,
        _capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_capabilities(
        &self,
        _device_id: Option<&str>,
    ) -> Result<ClientCapabilities, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_device(&self, _id: &str) -> Result<Option<DeviceInfoDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_devices(
        &self,
        _query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_device_infos(
        &self,
        _query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_devices_for_user(
        &self,
        _user_id: Option<Uuid>,
    ) -> Result<QueryResult<DeviceInfoDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn can_access_device(
        &self,
        _user: &UserEntity,
        _device_id: &str,
    ) -> Result<bool, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_device_options(
        &self,
        _device_id: &str,
        _device_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn get_device_options(
        &self,
        _device_id: &str,
    ) -> Result<Option<DeviceOptionsEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn to_client_capabilities_dto(
        &self,
        _capabilities: &ClientCapabilities,
    ) -> Result<ClientCapabilitiesDto, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`ClientEventLogger`]; unused by INFRA-level tests.
pub struct FakeClientEventLogger;

#[async_trait]
impl ClientEventLogger for FakeClientEventLogger {
    async fn write_document(
        &self,
        _client_name: &str,
        _client_version: &str,
        _contents: &[u8],
    ) -> Result<String, ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`ApiKeyManager`]; unused by INFRA-level tests.
pub struct FakeApiKeys;

#[async_trait]
impl ApiKeyManager for FakeApiKeys {
    async fn get_api_keys(&self) -> Result<Vec<AuthenticationInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_api_key(&self, _name: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn delete_api_key(&self, _access_token: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`LocalizationManager`]; every method is unused by INFRA-level tests.
pub struct FakeLocalization;

impl LocalizationManager for FakeLocalization {
    fn get_cultures(&self) -> Vec<CultureDto> {
        unimplemented!("fake")
    }
    fn get_countries(&self) -> Vec<CountryInfo> {
        unimplemented!("fake")
    }
    fn get_parental_ratings(&self) -> Vec<ParentalRating> {
        unimplemented!("fake")
    }
    fn get_localization_options(&self) -> Vec<LocalizationOption> {
        unimplemented!("fake")
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
    ) -> Option<ParentalRatingScore> {
        unimplemented!("fake")
    }
}

/// A fake [`DisplayPreferencesManager`]; every method is unused by INFRA-level tests.
pub struct FakeDisplayPreferences;

#[async_trait]
impl DisplayPreferencesManager for FakeDisplayPreferences {
    async fn get_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<DisplayPreferencesEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<ItemDisplayPreferencesEntity, ServiceError> {
        unimplemented!("fake")
    }
    async fn list_item_display_preferences(
        &self,
        _user_id: Uuid,
        _client: &str,
    ) -> Result<Vec<ItemDisplayPreferencesEntity>, ServiceError> {
        unimplemented!("fake")
    }
    async fn list_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<HashMap<String, Option<String>>, ServiceError> {
        unimplemented!("fake")
    }
    async fn set_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
        _custom_preferences: &HashMap<String, Option<String>>,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_display_preferences(
        &self,
        _display_preferences: &DisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_item_display_preferences(
        &self,
        _item_display_preferences: &ItemDisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`ActivityManager`]; every method is unused by INFRA-level tests.
pub struct FakeActivity;

#[async_trait]
impl ActivityManager for FakeActivity {
    async fn get_paged_result(
        &self,
        _query: &ActivityLogQuery,
    ) -> Result<QueryResult<ActivityLogEntry>, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_entry(
        &self,
        _entry: ferrofin_traits::activity::ActivityLogCreate,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn clean(&self, _before: chrono::DateTime<chrono::Utc>) -> Result<u64, ServiceError> {
        Ok(0)
    }
}

/// A recording [`ActivityManager`]: `create_entry` appends to `entries` so
/// tests can assert what a handler logged; the query surface is unused.
#[derive(Default)]
pub struct RecordingActivity {
    /// Every entry passed to `create_entry`, in call order.
    pub entries: std::sync::Mutex<Vec<ferrofin_traits::activity::ActivityLogCreate>>,
}

#[async_trait]
impl ActivityManager for RecordingActivity {
    async fn get_paged_result(
        &self,
        _query: &ActivityLogQuery,
    ) -> Result<QueryResult<ActivityLogEntry>, ServiceError> {
        unimplemented!("fake")
    }
    async fn create_entry(
        &self,
        entry: ferrofin_traits::activity::ActivityLogCreate,
    ) -> Result<(), ServiceError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }
    async fn clean(&self, _before: chrono::DateTime<chrono::Utc>) -> Result<u64, ServiceError> {
        Ok(0)
    }
}

/// A fake [`TaskManager`]; every method is unused by INFRA-level tests.
pub struct FakeTasks;

#[async_trait]
impl TaskManager for FakeTasks {
    async fn get_tasks(&self) -> Result<Vec<TaskInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_task(&self, _task_id: &str) -> Result<Option<TaskInfo>, ServiceError> {
        unimplemented!("fake")
    }
    async fn start_task(&self, _task_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn cancel_task(&self, _task_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn update_triggers(
        &self,
        _task_id: &str,
        _triggers: &[TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

/// A fake [`FileSystem`]; every method is unused by INFRA-level tests.
pub struct FakeFileSystem;

impl FileSystem for FakeFileSystem {
    fn get_file_system_entries(&self, _path: &str) -> Vec<FileSystemEntryInfo> {
        unimplemented!("fake")
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        unimplemented!("fake")
    }
    fn file_exists(&self, _path: &str) -> bool {
        unimplemented!("fake")
    }
    fn directory_exists(&self, _path: &str) -> bool {
        unimplemented!("fake")
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        unimplemented!("fake")
    }
    fn read_file(&self, _path: &str) -> Result<Vec<u8>, ServiceError> {
        unimplemented!("fake")
    }
}

/// A configurable in-memory [`VirtualFolderManager`] test double.
///
/// Models the on-disk virtual-folder tree as a `Vec<VirtualFolderInfo>` behind a
/// `Mutex`, with just enough behaviour to exercise the library-structure handlers
/// end-to-end (add/list/remove/rename/media-path/options) without touching a real
/// filesystem — `ferrofin-api` must never dev-depend on `ferrofin-core`. A `fail`
/// flag makes every operation return a [`ServiceError`] so error paths can be
/// probed too.
#[derive(Default)]
pub struct FakeVirtualFolders {
    /// The in-memory folders.
    folders: std::sync::Mutex<Vec<ferrofin_model::entities_media::VirtualFolderInfo>>,
    /// When set, every method fails with a backend error.
    fail: bool,
}

impl FakeVirtualFolders {
    /// A working (non-failing) fake, seeded empty.
    #[must_use]
    pub fn working() -> Self {
        Self::default()
    }

    /// A fake whose every method fails (to probe error mapping).
    #[must_use]
    pub fn failing() -> Self {
        Self {
            fail: true,
            folders: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The uniform failure used when [`Self::fail`] is set.
    fn guard(&self) -> Result<(), ServiceError> {
        if self.fail {
            Err(ServiceError::backend("fake virtual-folder failure"))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl VirtualFolderManager for FakeVirtualFolders {
    async fn get_virtual_folders(
        &self,
    ) -> Result<Vec<ferrofin_model::entities_media::VirtualFolderInfo>, ServiceError> {
        self.guard()?;
        Ok(self.folders.lock().expect("lock").clone())
    }

    async fn add_virtual_folder(
        &self,
        name: &str,
        collection_type: Option<ferrofin_model::entities::CollectionTypeOptions>,
        options: &ferrofin_model::configuration::LibraryOptions,
    ) -> Result<(), ServiceError> {
        self.guard()?;
        if name.trim().is_empty() {
            return Err(ServiceError::invalid_input("empty name"));
        }
        self.folders.lock().expect("lock").push(
            ferrofin_model::entities_media::VirtualFolderInfo {
                name: Some(name.to_owned()),
                locations: options.path_infos.iter().map(|p| p.path.clone()).collect(),
                collection_type,
                library_options: Some(options.clone()),
                ..ferrofin_model::entities_media::VirtualFolderInfo::default()
            },
        );
        Ok(())
    }

    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError> {
        self.guard()?;
        let mut folders = self.folders.lock().expect("lock");
        let before = folders.len();
        folders.retain(|f| f.name.as_deref() != Some(name));
        if folders.len() == before {
            return Err(ServiceError::not_found(name.to_owned()));
        }
        Ok(())
    }

    async fn rename_virtual_folder(&self, name: &str, new_name: &str) -> Result<(), ServiceError> {
        self.guard()?;
        let mut folders = self.folders.lock().expect("lock");
        if folders.iter().any(|f| f.name.as_deref() == Some(new_name)) {
            return Err(ServiceError::conflict(new_name.to_owned()));
        }
        let folder = folders
            .iter_mut()
            .find(|f| f.name.as_deref() == Some(name))
            .ok_or_else(|| ServiceError::not_found(name.to_owned()))?;
        folder.name = Some(new_name.to_owned());
        Ok(())
    }

    async fn add_media_path(
        &self,
        virtual_folder_name: &str,
        path_info: &ferrofin_model::configuration::MediaPathInfo,
    ) -> Result<(), ServiceError> {
        self.guard()?;
        let mut folders = self.folders.lock().expect("lock");
        let folder = folders
            .iter_mut()
            .find(|f| f.name.as_deref() == Some(virtual_folder_name))
            .ok_or_else(|| ServiceError::not_found(virtual_folder_name.to_owned()))?;
        folder.locations.push(path_info.path.clone());
        Ok(())
    }

    async fn update_media_path(
        &self,
        virtual_folder_name: &str,
        _path_info: &ferrofin_model::configuration::MediaPathInfo,
    ) -> Result<(), ServiceError> {
        self.guard()?;
        let folders = self.folders.lock().expect("lock");
        if folders
            .iter()
            .any(|f| f.name.as_deref() == Some(virtual_folder_name))
        {
            Ok(())
        } else {
            Err(ServiceError::not_found(virtual_folder_name.to_owned()))
        }
    }

    async fn remove_media_path(
        &self,
        virtual_folder_name: &str,
        path: &str,
    ) -> Result<(), ServiceError> {
        self.guard()?;
        let mut folders = self.folders.lock().expect("lock");
        let folder = folders
            .iter_mut()
            .find(|f| f.name.as_deref() == Some(virtual_folder_name))
            .ok_or_else(|| ServiceError::not_found(virtual_folder_name.to_owned()))?;
        folder.locations.retain(|l| l != path);
        Ok(())
    }

    async fn update_library_options(
        &self,
        virtual_folder_name: &str,
        options: &ferrofin_model::configuration::LibraryOptions,
    ) -> Result<(), ServiceError> {
        self.guard()?;
        let mut folders = self.folders.lock().expect("lock");
        let folder = folders
            .iter_mut()
            .find(|f| f.name.as_deref() == Some(virtual_folder_name))
            .ok_or_else(|| ServiceError::not_found(virtual_folder_name.to_owned()))?;
        folder.library_options = Some(options.clone());
        Ok(())
    }
}

/// Builds an [`AppState`] whose virtual-folder store is `vf` and whose auth
/// always authenticates ([`AuthedAuthService`]), so the library-structure
/// handlers are reached (rather than short-circuited with `401`).
///
/// Every other manager is the panic-on-call [`fake_state`] double — the
/// library-structure handlers touch only the virtual-folder store and the auth
/// service.
#[must_use]
pub fn authed_state_with_virtual_folders(vf: Arc<dyn VirtualFolderManager>) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(AuthedAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
    .with_virtual_folders(vf)
}

/// Like [`authed_state_with_virtual_folders`] but authenticated as an API key,
/// for `GET /Library/PhysicalPaths` and the other elevated library routes.
#[must_use]
pub fn elevated_state_with_virtual_folders(vf: Arc<dyn VirtualFolderManager>) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(ApiKeyAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
    .with_virtual_folders(vf)
}

/// Builds an authenticating [`AppState`] with the given plugin manager injected,
/// for the `/Plugins/*`, `/Packages/*` and `/Repositories` handler tests.
///
/// Every other manager is the [`fake_state`] double; authentication succeeds
/// as an **API key** ([`ApiKeyAuthService`]) so both the plain-auth reads and
/// the admin-gated mutators reach their handler.
#[must_use]
pub fn authed_state_with_plugins(
    plugins: Arc<dyn ferrofin_traits::plugins::PluginManager>,
) -> AppState {
    plugin_state(plugins, Arc::new(ApiKeyAuthService))
}

/// Like [`authed_state_with_plugins`], but authenticated as a plain user with
/// **no** admin policy — for proving the plugin-mutating routes 403.
#[must_use]
pub fn user_authed_state_with_plugins(
    plugins: Arc<dyn ferrofin_traits::plugins::PluginManager>,
) -> AppState {
    plugin_state(plugins, Arc::new(AuthedAuthService))
}

fn plugin_state(
    plugins: Arc<dyn ferrofin_traits::plugins::PluginManager>,
    auth: Arc<dyn AuthService>,
) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        auth,
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
    .with_plugins(plugins)
}

/// Builds an authenticating [`AppState`] for the subtitle-conversion +
/// FallbackFont routes, injecting the four managers those handlers touch
/// (`library`, `config`, `file_system`, `media_sources`) and the subtitle
/// encoder.
///
/// Every other manager is the panic-on-call [`fake_state`] double, so a handler
/// that strays outside those seams is caught. Authentication always succeeds
/// ([`AuthedAuthService`]) so the `RequireAuth`-guarded FallbackFont/playlist
/// routes reach their handler.
#[must_use]
pub fn authed_state_for_subtitles(
    library: Arc<dyn LibraryManager>,
    config: Arc<dyn ServerConfigurationManager>,
    file_system: Arc<dyn FileSystem>,
    media_sources: Arc<dyn MediaSourceManager>,
    subtitle_encoder: Arc<dyn SubtitleEncoder>,
) -> AppState {
    AppState::new(
        library,
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        media_sources,
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        config,
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(AuthedAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        file_system,
        Arc::new(FakeTasks),
    )
    .with_subtitle_encoder(subtitle_encoder)
}

/// Builds an [`AppState`] whose library manager is `library` and whose library
/// monitor is `monitor`, with always-authenticating auth so the change-report
/// webhooks (`/Library/Movies/*`, `/Library/Series/*`, `/Library/Media/Updated`)
/// are reached.
///
/// Every other manager is the panic-on-call [`fake_state`] double — those
/// webhooks touch only the library manager, the library monitor and the auth
/// service.
#[must_use]
pub fn authed_state_with_library_and_monitor(
    library: Arc<dyn LibraryManager>,
    monitor: Arc<dyn LibraryMonitor>,
) -> AppState {
    AppState::new(
        library,
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(AuthedAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
    .with_library_monitor(monitor)
}

/// Like [`authed_state_with_library_and_monitor`] but authenticated as an API
/// key, for `POST /Library/Refresh`.
#[must_use]
pub fn elevated_state_with_library_and_monitor(
    library: Arc<dyn LibraryManager>,
    monitor: Arc<dyn LibraryMonitor>,
) -> AppState {
    AppState::new(
        library,
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(ApiKeyAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
    .with_library_monitor(monitor)
}

/// Builds an [`AppState`] whose library manager is `library` and whose provider
/// manager is `providers`, with always-authenticating auth.
///
/// Used by the remote-metadata search + apply routes (`/Items/RemoteSearch/*`),
/// which touch only the provider manager (for the search / refresh seam) and the
/// library manager (to resolve the item on `Apply`). Every other manager is the
/// panic-on-call [`fake_state`] double.
#[must_use]
pub fn authed_state_with_library_and_providers(
    library: Arc<dyn LibraryManager>,
    providers: Arc<dyn ProviderManager>,
) -> AppState {
    AppState::new(
        library,
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        providers,
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(AuthedAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Like [`authed_state_with_library_and_providers`] but authenticated as an
/// **API key**, which satisfies `RequiresElevation` — for the library and
/// item-lookup routes that are admin-only upstream.
#[must_use]
pub fn elevated_state_with_library_and_providers(
    library: Arc<dyn LibraryManager>,
    providers: Arc<dyn ProviderManager>,
) -> AppState {
    AppState::new(
        library,
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        providers,
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(ApiKeyAuthService),
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
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}
