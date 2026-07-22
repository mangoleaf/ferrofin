//! Library-layer **manager** traits — the orchestration seam.
//!
//! Port of the `ILibraryManager` / `IUser*Manager` / `IMediaSourceManager` /
//! `ISearchManager` / `IMusicManager` / `ILibraryMonitor` /
//! `ISimilarItemsManager` interfaces in `MediaBrowser.Controller.Library`.
//! Managers coordinate business logic and delegate raw row access to the
//! [`crate::persistence`] repositories.
//!
//! Port rules applied throughout:
//! - The C# `BaseItem`/`Folder`/`Video`/`User` OOP domain hierarchy is **not**
//!   ported. Identity arguments become [`uuid::Uuid`]; items returned from a
//!   query become [`BaseItemEntity`] rows; user arguments become [`UserEntity`]
//!   rows; DTO-shaped results reuse `hermit-model` DTOs.
//! - Method **overloads** collapse to a single method (e.g. the many
//!   `GetItemList` overloads become one `get_item_list`).
//! - Resolver/path/sort/named-view/OOP-tree methods that only make sense with
//!   the un-ported domain tree (`ResolvePath`, `GetArtist`, `Sort`,
//!   `GetNamedView`, `ParseName`, …) are dropped here; they resurface as
//!   `hermit-core` free functions in Wave 6.
//! - `Task<T>` becomes `async fn -> Result<T, ServiceError>`; `IProgress` /
//!   `CancellationToken` are dropped for v1.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion,
//! because `AppState` stores each behind `Arc<dyn _>`.

use async_trait::async_trait;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::UserConfiguration;
use hermit_model::data::CollectionType;
use hermit_model::dto::{
    ItemCounts, MediaSourceInfo, NameIdPair, RecommendationType, UpdateUserItemDataDto,
    UserItemDataDto,
};
use hermit_model::media_info::LiveStreamRequest;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_model::search::{SearchHint, SearchQuery};
use hermit_model::users::UserPolicy;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::{DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery};
use crate::persistence::ItemWithCounts;

/// A search match: an item id paired with a relevance score.
///
/// Port of `MediaBrowser.Controller.Library.SearchResult`; the C# `Guid ItemId`
/// becomes a [`Uuid`] and the `float Score` an [`f32`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchResult {
    /// The id of the matching item.
    pub item_id: Uuid,
    /// The relevance score; higher is more relevant.
    pub score: f32,
}

/// A search hint (an item row) with the term that matched it.
///
/// Port of `MediaBrowser.Controller.Library.SearchHintInfo`; the domain
/// `BaseItem Item` becomes a [`BaseItemEntity`] row.
#[derive(Debug, Clone)]
pub struct SearchHintInfo {
    /// The matched item row.
    pub item: BaseItemEntity,
    /// The term that matched.
    pub matched_term: String,
}

/// A recommendation category derived from a baseline item.
///
/// Port of `MediaBrowser.Controller.Library.SimilarItemsRecommendation`; the
/// domain `IReadOnlyList<BaseItem>` becomes `Vec<BaseItemEntity>`.
#[derive(Debug, Clone)]
pub struct SimilarItemsRecommendation {
    /// The display name of the baseline item.
    pub baseline_item_name: String,
    /// An identifier for the recommendation category.
    pub category_id: Uuid,
    /// The recommendation type.
    pub recommendation_type: RecommendationType,
    /// The similar items, ordered by relevance.
    pub items: Vec<BaseItemEntity>,
}

/// Orchestrates the item library: queries, counts, people, genres, deletion.
///
/// Port of `ILibraryManager` (the object-safe, domain-tree-free subset). The
/// resolver/path/sort/named-view methods are intentionally omitted — they
/// depend on the un-ported C# `BaseItem` hierarchy and become `hermit-core`
/// free functions in Wave 6.
#[async_trait]
pub trait LibraryManager: Send + Sync {
    /// Gets a single item row by id, or `None` if it does not exist.
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError>;

    /// Runs a query and returns a page of item rows plus the total count.
    async fn query_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError>;

    /// Returns just the ids of the items matching the query.
    async fn get_item_ids(&self, query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError>;

    /// Returns the full (unpaginated) list of item rows matching the query.
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Returns the latest item rows for the given collection type.
    async fn get_latest_item_list(
        &self,
        query: &InternalItemsQuery,
        collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Persists (inserts or updates) the given item rows under a parent.
    async fn create_items(
        &self,
        items: &[BaseItemEntity],
        parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError>;

    /// Updates the given item rows under a parent.
    async fn update_items(
        &self,
        items: &[BaseItemEntity],
        parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError>;

    /// Deletes an item, honoring the given [`DeleteOptions`].
    async fn delete_item(&self, id: Uuid, options: &DeleteOptions) -> Result<(), ServiceError>;

    /// Gets the people rows attached to an item.
    async fn get_people(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError>;

    /// Gets the distinct people names matching a query.
    async fn get_people_names(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError>;

    /// Counts the items matching the query.
    async fn get_count(&self, query: &InternalItemsQuery) -> Result<i32, ServiceError>;

    /// Gets item counts grouped by kind for the query.
    async fn get_item_counts(&self, query: &InternalItemsQuery)
    -> Result<ItemCounts, ServiceError>;

    /// Gets genres with their item counts.
    async fn get_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets studios with their item counts.
    async fn get_studios(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets artists with their item counts.
    async fn get_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets aggregated legacy query-filter values for the matching items.
    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError>;

    /// Queues a full library scan.
    async fn queue_library_scan(&self) -> Result<(), ServiceError>;
}

fn _assert_object_safe_library_manager(_: &dyn LibraryManager) {}

/// Manages user accounts, authentication, and per-user policy/configuration.
///
/// Port of `IUserManager`. User rows are [`UserEntity`]; the `GetUserDto`
/// method is deliberately omitted until `UserDto` lands in `hermit-model`
/// (flagged in the port report).
#[async_trait]
pub trait UserManager: Send + Sync {
    /// Gets all user rows.
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError>;

    /// Gets the ids of all users.
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError>;

    /// Ensures at least one user exists (first-run bootstrap).
    async fn initialize(&self) -> Result<(), ServiceError>;

    /// Gets a user row by id, or `None`.
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError>;

    /// Gets the first available user row, or `None`.
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError>;

    /// Gets a user row by name, or `None`.
    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserEntity>, ServiceError>;

    /// Renames a user.
    async fn rename_user(
        &self,
        user_id: Uuid,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), ServiceError>;

    /// Persists changes to a user row.
    async fn update_user(&self, user: &UserEntity) -> Result<(), ServiceError>;

    /// Creates a user with the given name and returns the new row.
    async fn create_user(&self, name: &str) -> Result<UserEntity, ServiceError>;

    /// Deletes a user by id.
    async fn delete_user(&self, user_id: Uuid) -> Result<(), ServiceError>;

    /// Resets a user's password to empty.
    async fn reset_password(&self, user_id: Uuid) -> Result<(), ServiceError>;

    /// Changes a user's password.
    async fn change_password(&self, user_id: Uuid, new_password: &str) -> Result<(), ServiceError>;

    /// Authenticates a user by name/password, returning the row on success.
    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
        remote_endpoint: &str,
        is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError>;

    /// Lists the available authentication providers.
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError>;

    /// Lists the available password-reset providers.
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError>;

    /// Updates a user's configuration (stopgap; prefer [`Self::update_user`]).
    async fn update_configuration(
        &self,
        user_id: Uuid,
        config: &UserConfiguration,
    ) -> Result<(), ServiceError>;

    /// Updates a user's policy (stopgap; prefer [`Self::update_user`]).
    async fn update_policy(&self, user_id: Uuid, policy: &UserPolicy) -> Result<(), ServiceError>;

    /// Clears a user's profile image.
    async fn clear_profile_image(&self, user: &UserEntity) -> Result<(), ServiceError>;
}

fn _assert_object_safe_user_manager(_: &dyn UserManager) {}

/// Reads and writes per-user, per-item playback/rating data.
///
/// Port of `IUserDataManager`. User/item arguments become [`Uuid`] identities;
/// results are the [`UserItemDataDto`] presentation DTO. The C# `event
/// UserDataSaved` is dropped (events are wired separately in `hermit-core`).
#[async_trait]
pub trait UserDataManager: Send + Sync {
    /// Saves user data supplied as an update DTO.
    async fn save_user_data(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError>;

    /// Gets the presentation DTO of a user's data for an item, or `None`.
    async fn get_user_data_dto(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError>;

    /// Gets user-data DTOs for several items in one batch.
    async fn get_user_data_batch(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError>;

    /// Updates play state from a reported position, returning whether the item
    /// is now considered played to completion.
    async fn update_play_state(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError>;

    /// Clears remembered audio/subtitle stream selections for a user/item pair.
    async fn reset_playback_stream_selections(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_user_data_manager(_: &dyn UserDataManager) {}

/// Builds the per-user "views" (home rows / latest sections).
///
/// Port of `IUserViewManager`. The domain `Folder`/`UserView` returns become
/// [`BaseItemEntity`] rows; the C# query params become plain arguments.
#[async_trait]
pub trait UserViewManager: Send + Sync {
    /// Gets the top-level views for a user.
    async fn get_user_views(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Gets latest items grouped per parent view.
    async fn get_latest_items(
        &self,
        user_id: Uuid,
        options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError>;
}

fn _assert_object_safe_user_view_manager(_: &dyn UserViewManager) {}

/// Resolves and opens playable media sources for an item.
///
/// Port of `IMediaSourceManager` (the API-facing subset). Streams and sources
/// surface as `hermit-model` DTOs at this layer; the `AddParts`/live-stream
/// direct-provider internals are dropped for v1.
#[async_trait]
pub trait MediaSourceManager: Send + Sync {
    /// Gets the media streams of an item as presentation DTOs.
    async fn get_media_streams(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<hermit_model::entities_media::MediaStream>, ServiceError>;

    /// Gets the media attachments of an item as presentation DTOs.
    async fn get_media_attachments(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<hermit_model::entities_media::MediaAttachment>, ServiceError>;

    /// Gets the playback media sources for an item and user.
    async fn get_playback_media_sources(
        &self,
        item_id: Uuid,
        user_id: Uuid,
        allow_media_probe: bool,
        enable_path_substitution: bool,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError>;

    /// Gets the static (non-probed) media sources for an item.
    async fn get_static_media_sources(
        &self,
        item_id: Uuid,
        enable_path_substitution: bool,
        user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError>;

    /// Opens a live stream and returns its media source.
    async fn open_live_stream(
        &self,
        request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError>;

    /// Gets an already-open live stream's media source by id.
    async fn get_live_stream(&self, id: &str) -> Result<MediaSourceInfo, ServiceError>;

    /// Closes an open live stream.
    async fn close_live_stream(&self, id: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_media_source_manager(_: &dyn MediaSourceManager) {}

/// Orchestrates search across registered providers.
///
/// Port of `ISearchManager`. The `AddParts`/`GetProviders` provider-registry
/// methods are dropped (registration is `hermit-core`'s job); results reuse
/// [`SearchHint`] and [`SearchResult`].
#[async_trait]
pub trait SearchManager: Send + Sync {
    /// Gets ranked search hints for autocomplete/typeahead.
    async fn get_search_hints(
        &self,
        query: &SearchQuery,
    ) -> Result<QueryResult<SearchHint>, ServiceError>;

    /// Gets ranked (id, score) search results for a provider query.
    async fn get_search_results(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, ServiceError>;
}

fn _assert_object_safe_search_manager(_: &dyn SearchManager) {}

/// Builds "instant mix" playlists from a seed.
///
/// Port of `IMusicManager`. The domain `BaseItem`/`MusicArtist` seeds become
/// [`Uuid`] identities; results are [`BaseItemEntity`] rows.
#[async_trait]
pub trait MusicManager: Send + Sync {
    /// Builds an instant mix seeded by an item.
    async fn get_instant_mix_from_item(
        &self,
        item_id: Uuid,
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Builds an instant mix seeded by an artist item.
    async fn get_instant_mix_from_artist(
        &self,
        artist_id: Uuid,
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Builds an instant mix seeded by genre names.
    async fn get_instant_mix_from_genres(
        &self,
        genres: &[String],
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;
}

fn _assert_object_safe_music_manager(_: &dyn MusicManager) {}

/// Watches library filesystems for changes.
///
/// Port of `ILibraryMonitor`. The C# methods are synchronous file-watcher
/// hooks; they stay `async fn -> Result` here so implementations may do I/O and
/// surface failures uniformly.
#[async_trait]
pub trait LibraryMonitor: Send + Sync {
    /// Starts monitoring.
    async fn start(&self) -> Result<(), ServiceError>;

    /// Stops monitoring.
    async fn stop(&self) -> Result<(), ServiceError>;

    /// Signals that a change at `path` is beginning (suppress self-triggering).
    async fn report_file_system_change_beginning(&self, path: &str) -> Result<(), ServiceError>;

    /// Signals that a change at `path` is complete, optionally refreshing it.
    async fn report_file_system_change_complete(
        &self,
        path: &str,
        refresh_path: bool,
    ) -> Result<(), ServiceError>;

    /// Signals that `path` changed on disk.
    async fn report_file_system_changed(&self, path: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_library_monitor(_: &dyn LibraryMonitor) {}

/// Finds items similar to a seed and builds recommendation categories.
///
/// Port of `ISimilarItemsManager`. The generic `GetSimilarItemsProviders<T>` and
/// `AddParts` registry methods are dropped; similar-item results become
/// [`BaseItemEntity`] rows and recommendations [`SimilarItemsRecommendation`].
#[async_trait]
pub trait SimilarItemsManager: Send + Sync {
    /// Gets items similar to `item_id`.
    async fn get_similar_items(
        &self,
        item_id: Uuid,
        exclude_artist_ids: &[Uuid],
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
        limit: Option<i32>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Builds movie recommendation categories for a user.
    async fn get_movie_recommendations(
        &self,
        user_id: Option<Uuid>,
        parent_id: Uuid,
        category_limit: i32,
        item_limit: i32,
        dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError>;
}

fn _assert_object_safe_similar_items_manager(_: &dyn SimilarItemsManager) {}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{SearchResult, SimilarItemsRecommendation};
    use hermit_model::dto::RecommendationType;

    #[test]
    fn search_result_holds_id_and_score() {
        let id = Uuid::nil();
        let r = SearchResult {
            item_id: id,
            score: 0.5,
        };
        assert_eq!(r.item_id, id);
        assert!((r.score - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn recommendation_carries_baseline_and_items() {
        let rec = SimilarItemsRecommendation {
            baseline_item_name: "Because you watched".to_owned(),
            category_id: Uuid::nil(),
            recommendation_type: RecommendationType::SimilarToLikedItem,
            items: Vec::new(),
        };
        assert_eq!(rec.baseline_item_name, "Because you watched");
        assert!(rec.items.is_empty());
    }
}
