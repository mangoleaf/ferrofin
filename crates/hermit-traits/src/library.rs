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
use hermit_model::data::{BaseItemKind, CollectionType};
use hermit_model::dto::{
    ItemCounts, MediaSourceInfo, NameIdPair, RecommendationType, UpdateUserItemDataDto, UserDto,
    UserItemDataDto,
};
use hermit_model::entities::MediaStreamType;
use hermit_model::media_info::LiveStreamRequest;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_model::search::{SearchHint, SearchQuery};
use hermit_model::users::UserPolicy;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::{
    DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery, ItemImageInfo,
};
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

    /// Gets the image rows attached to an item.
    ///
    /// Port of the `BaseItem.ImageInfos` accessor the image controllers read
    /// before serving or projecting an item's images; the concrete manager
    /// delegates to [`ItemRepository::get_image_infos`](crate::persistence::ItemRepository::get_image_infos).
    /// An item with no images yields an empty vector.
    ///
    /// The default is the no-image fallback (an empty vector), so impls that do
    /// not store images — test doubles and managers without a persistence seam —
    /// compile unchanged; the concrete manager overrides it with the real read.
    async fn get_item_images(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
        let _ = item_id;
        Ok(Vec::new())
    }

    /// Gets an item's ancestor rows, nearest parent first, walking the
    /// `ParentId` chain up to the root.
    ///
    /// Port of the `BaseItem.GetParents()` walk that `LibraryController.GetAncestors`
    /// consumes: starting from the item's parent, each row's [`parent_id`] is
    /// followed until it is absent or no longer resolves. The seed item itself is
    /// not included. A missing seed item yields [`None`] so the controller can map
    /// it to a `404`; a resolvable item with no parent yields an empty list.
    ///
    /// The default folds [`Self::get_item_by_id`], so every impl gets the walk for
    /// free. A `parent_id` that points back into the already-visited set is
    /// treated as the end of the chain, guarding against a cyclic `ParentId`.
    ///
    /// [`parent_id`]: hermit_db::entities::base_items::BaseItemEntity::parent_id
    async fn get_ancestors(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        let Some(item) = self.get_item_by_id(item_id).await? else {
            return Ok(None);
        };
        let mut ancestors = Vec::new();
        let mut seen = vec![item_id];
        let mut next = item
            .parent_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok());
        while let Some(parent_id) = next {
            if seen.contains(&parent_id) {
                break;
            }
            let Some(parent) = self.get_item_by_id(parent_id).await? else {
                break;
            };
            seen.push(parent_id);
            next = parent
                .parent_id
                .as_deref()
                .and_then(|p| Uuid::parse_str(p).ok());
            ancestors.push(parent);
        }
        Ok(Some(ancestors))
    }

    /// Gets the user root folder row — the synthetic top of the library tree
    /// that `Items/Root` (and the `itemId.IsEmpty()` fallbacks across the
    /// user-library controller) resolve to, or `None` if it has not been
    /// materialized.
    ///
    /// Port of `ILibraryManager.GetUserRootFolder`. Jellyfin lazily creates the
    /// [`BaseItemKind::UserRootFolder`] on disk; that filesystem side effect is
    /// out of scope for this portable seam, so the default resolves the single
    /// persisted `UserRootFolder` row (the first one, mirroring C#
    /// `FirstOrDefault`) via [`Self::get_item_list`] and reports `None` when
    /// absent.
    async fn get_user_root_folder(&self) -> Result<Option<BaseItemEntity>, ServiceError> {
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::UserRootFolder],
            ..InternalItemsQuery::default()
        };
        Ok(self.get_item_list(&query).await?.into_iter().next())
    }

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

    /// Merges several videos into one version group.
    ///
    /// Port of `VideosController.MergeVersions`: picks a primary version among
    /// `ids` (preferring one that already owns multiple sources, else the best by
    /// video type / resolution) and links every other supplied id to it by
    /// setting its `PrimaryVersionId`. Returns [`ServiceError::InvalidInput`] when
    /// fewer than two distinct, resolvable videos are supplied.
    ///
    /// The C# `LinkedAlternateVersions` array + linked-child reroute are not
    /// modeled at this seam (Hermit tracks the version group solely by each row's
    /// `PrimaryVersionId` pointer); setting that pointer is the portable core of
    /// the merge.
    ///
    /// The default implementation reports the operation as unsupported, so a
    /// manager that does not persist version groups need not override it; the
    /// concrete `HermitLibraryManager` does.
    async fn merge_versions(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        let _ = ids;
        Err(ServiceError::backend("merge_versions not supported"))
    }

    /// Removes the alternate-version links of a video (and of its whole group).
    ///
    /// Port of `VideosController.DeleteAlternateSources`: resolves the item's
    /// primary version, then clears the `PrimaryVersionId` pointer on the primary
    /// and on every item linked to it, so each becomes a standalone version again.
    /// Returns [`ServiceError::NotFound`] when the item does not exist.
    ///
    /// The default implementation reports the operation as unsupported (see
    /// [`merge_versions`](Self::merge_versions)); `HermitLibraryManager` overrides
    /// it.
    async fn remove_alternate_sources(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let _ = item_id;
        Err(ServiceError::backend(
            "remove_alternate_sources not supported",
        ))
    }

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

    /// Gets music genres with their item counts.
    ///
    /// Port of `ILibraryManager.GetMusicGenres`. Unlike [`Self::get_genres`],
    /// this counts against the music-genre by-name kind, so the music-library
    /// browse (`GET /MusicGenres`) and the music-collection branch of
    /// `GET /Genres` resolve the same rows Jellyfin does.
    async fn get_music_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets album artists with their item counts.
    ///
    /// Port of `ILibraryManager.GetAlbumArtists`. Restricts the by-name artist
    /// rows to those referenced as *album* artists, backing
    /// `GET /Artists/AlbumArtists`.
    async fn get_album_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Resolves a single by-name item (genre, studio, artist, person, year, …)
    /// of the given [`BaseItemKind`] by its name, or `None` when no such row
    /// exists.
    ///
    /// Port of `ILibraryManager`'s by-name resolvers (`GetGenre`, `GetStudio`,
    /// `GetArtist`, `GetPerson`, `GetMusicGenre`, `GetYear`). Jellyfin's
    /// versions create the backing folder on disk when absent; that filesystem
    /// side effect is out of scope for this portable seam, so a missing item is
    /// reported as `None` and each controller applies its own empty/`404`
    /// fallback. Matching is by cleaned name (Jellyfin's item-by-name id is
    /// derived from the name), delegating to [`Self::get_item_list`] filtered to
    /// `kind`; the first match wins, mirroring C# `FirstOrDefault`.
    async fn get_named_item(
        &self,
        kind: BaseItemKind,
        name: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        let query = InternalItemsQuery {
            name: Some(name.to_owned()),
            include_item_types: vec![kind],
            ..InternalItemsQuery::default()
        };
        Ok(self.get_item_list(&query).await?.into_iter().next())
    }

    /// Resolves the people matching `query` to their by-name `Person` item rows.
    ///
    /// Port of `ILibraryManager.GetPeopleItems`: it fetches the credited people
    /// via [`Self::get_people`], then resolves each name to its `Person`
    /// [`BaseItemEntity`] (dropping any that no longer resolve), preserving the
    /// people query's paging. The default folds the two calls so every impl gets
    /// it for free from [`Self::get_people`] + [`Self::get_named_item`].
    async fn get_people_items(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        let people = self.get_people(query).await?;
        let mut items = Vec::with_capacity(people.len());
        for person in people {
            if let Some(item) = self
                .get_named_item(BaseItemKind::Person, &person.name)
                .await?
            {
                items.push(item);
            }
        }
        Ok(QueryResult::new(
            query.start_index,
            Some(i32::try_from(items.len()).unwrap_or(i32::MAX)),
            items,
        ))
    }

    /// Gets the library's production years, resolved to their by-name `Year`
    /// item rows, sorted ascending and paged by `start_index`/`limit`.
    ///
    /// Port of `YearsController.GetYears`: Jellyfin walks the (localized) item
    /// tree, collects each item's distinct `ProductionYear`, and resolves each
    /// to a `Year` item. Here the distinct years come from
    /// [`Self::get_query_filters_legacy`] over the same `query`, and each is
    /// resolved via [`Self::get_named_item`]; years without a materialized row
    /// are skipped (Jellyfin's `.Where(i => i is not null)`), since on-disk
    /// creation is out of scope for this portable seam.
    async fn get_years(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        let mut years = self.get_query_filters_legacy(query).await?.years;
        years.retain(|y| *y > 0);
        years.sort_unstable();
        years.dedup();
        let start = usize::try_from(query.start_index.unwrap_or(0).max(0)).unwrap_or(0);
        let mut items = Vec::new();
        for year in years.into_iter().skip(start) {
            if let Some(limit) = query.limit
                && limit >= 0
                && items.len() >= usize::try_from(limit).unwrap_or(usize::MAX)
            {
                break;
            }
            if let Some(item) = self
                .get_named_item(BaseItemKind::Year, &year.to_string())
                .await?
            {
                items.push(item);
            }
        }
        Ok(QueryResult::new(
            query.start_index,
            Some(i32::try_from(items.len()).unwrap_or(i32::MAX)),
            items,
        ))
    }

    /// Gets aggregated legacy query-filter values for the matching items.
    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError>;

    /// Gets the distinct language codes of the matching items' media streams of
    /// a given [`MediaStreamType`].
    ///
    /// Port of `ILibraryManager.GetMediaStreamLanguages(MediaStreamType,
    /// InternalItemsQuery)`, which backs the audio/subtitle language facets of
    /// `GET /Items/Filters2`. The distinct codes come from the query's matching
    /// items' streams; an empty language is normalized to `"und"` (undetermined)
    /// exactly as Jellyfin does.
    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
        query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError>;

    /// Queues a full library scan.
    async fn queue_library_scan(&self) -> Result<(), ServiceError>;
}

fn _assert_object_safe_library_manager(_: &dyn LibraryManager) {}

/// Manages user accounts, authentication, and per-user policy/configuration.
///
/// Port of `IUserManager`. User rows are [`UserEntity`]; [`Self::get_user_dto`]
/// projects a row into the public [`UserDto`] (policy + configuration).
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

    /// Projects a user row into the full public [`UserDto`].
    ///
    /// Port of `UserManager.GetUserDto`: assembles the user's
    /// [`UserConfiguration`](hermit_model::configuration::UserConfiguration) and
    /// [`UserPolicy`](hermit_model::users::UserPolicy) from the `Users` row plus
    /// its `Permissions`/`Preferences`/`AccessSchedules`. `server_id` is the
    /// hosting application's system id; `remote_endpoint` is accepted for parity
    /// (the profile-image cache tag it feeds is not yet ported).
    async fn get_user_dto(
        &self,
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<UserDto, ServiceError>;

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

    /// Stores caller-supplied profile-image bytes for a user.
    ///
    /// Port of the `POST /UserImage` tail: clear any existing profile image, write
    /// the decoded bytes to the user's `profile{extension}` path, and persist the
    /// user (`_providerManager.SaveImage(stream, mime, path)` +
    /// `UpdateUserAsync`). `extension` is the image extension derived from the
    /// upload `Content-Type` (e.g. `.png`).
    ///
    /// The default implementation reports the image pipeline as deferred (as the
    /// shell provider manager does for `save_image`), so impls without a
    /// profile-image store compile unchanged; the concrete manager overrides it.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] while the image store is deferred, or whatever
    /// error the concrete write surfaces.
    async fn save_profile_image(
        &self,
        user: &UserEntity,
        content: &[u8],
        mime_type: &str,
        extension: &str,
    ) -> Result<(), ServiceError> {
        let _ = (user, content, mime_type, extension);
        Err(ServiceError::backend(
            "save_profile_image is deferred until the image pipeline lands",
        ))
    }

    /// Gets a user's profile image (`ImageInfos` row), or `None` when the user
    /// has no profile image set.
    ///
    /// Port of the `User.ProfileImage` accessor the image controller reads before
    /// serving `GET /UserImage`. The returned [`ItemImageInfo`] carries the
    /// stored path, last-modified time, and a [`ImageType::Profile`] type; width,
    /// height, and blurhash are unknown for user images and left at their
    /// defaults.
    ///
    /// [`ImageType::Profile`]: hermit_model::entities::ImageType::Profile
    ///
    /// The default is the no-image fallback ([`None`]), so impls without a
    /// profile-image store compile unchanged; the concrete manager overrides it.
    async fn get_profile_image(
        &self,
        user_id: Uuid,
    ) -> Result<Option<ItemImageInfo>, ServiceError> {
        let _ = user_id;
        Ok(None)
    }
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

    /// Marks an item as played for a user, returning the refreshed data DTO.
    ///
    /// Port of `BaseItem.MarkPlayed`: sets `Played`, resets the resume position,
    /// stamps `LastPlayedDate` (defaulting to now), and — when `date_played` is
    /// supplied — increments `PlayCount` (always at least one).
    async fn mark_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError>;

    /// Marks an item as unplayed for a user, returning the refreshed data DTO.
    ///
    /// Port of `BaseItem.MarkUnplayed` / `ResetPlayedState`: clears `Played`,
    /// the play count, the resume position, and `LastPlayedDate`.
    async fn mark_unplayed(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError>;

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
