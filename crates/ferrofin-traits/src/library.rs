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
//!   rows; DTO-shaped results reuse `ferrofin-model` DTOs.
//! - Method **overloads** collapse to a single method (e.g. the many
//!   `GetItemList` overloads become one `get_item_list`).
//! - Resolver/path/sort/named-view/OOP-tree methods that only make sense with
//!   the un-ported domain tree (`ResolvePath`, `GetArtist`, `Sort`,
//!   `GetNamedView`, `ParseName`, …) are dropped here; they resurface as
//!   `ferrofin-core` free functions in Wave 6.
//! - `Task<T>` becomes `async fn -> Result<T, ServiceError>`; `IProgress` /
//!   `CancellationToken` are dropped for v1.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion,
//! because `AppState` stores each behind `Arc<dyn _>`.

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, UserConfiguration};
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::{
    ItemCounts, MediaSourceInfo, NameIdPair, RecommendationType, SortOrder, UpdateUserItemDataDto,
    UserDto, UserItemDataDto,
};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities::{ImageType, MediaStreamType, MetadataField};
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::media_info::LiveStreamRequest;
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_model::search::{SearchHint, SearchQuery};
use ferrofin_model::users::UserPolicy;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::{
    DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery, ItemImageInfo,
};
use crate::persistence::ItemWithCounts;
use crate::providers::MetadataRefreshOptions;

/// Where a scan reports how far it got, in percent (0–100) — the
/// `IProgress<double>` a scheduled task hands its work.
pub type ScanProgressSink = std::sync::Arc<dyn Fn(f64) + Send + Sync>;

/// Why a library scan runs — the request that started it: the `trigger` field
/// of its `library_scan` span and log lines (`docs/conventions/LOGGING.md`'s
/// vocabulary) and the bounded `trigger` label of the library-scan metrics
/// (`contrib/metrics/README.md`). A label only: requests that coalesce into
/// one scan run as one, under the trigger of the request that scan runs as —
/// the queued request the later ones joined, or a queued full scan that took
/// over the pending requests it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanTrigger {
    /// A request over the API: `POST /Library/Refresh`, an item's or a
    /// folder's refresh, a library change, and the "Scan Media Library" task
    /// started from the dashboard ("Scan All Libraries").
    Api,
    /// The "Scan Media Library" task's own interval, daily or weekly trigger.
    Schedule,
    /// The "Scan Media Library" task's startup trigger.
    Startup,
    /// The filesystem watcher (real-time monitoring).
    Watcher,
    /// An *arr webhook: `POST /Library/Series/Added`, `/Library/Series/Updated`,
    /// `/Library/Movies/Added`, `/Library/Movies/Updated`,
    /// `/Library/Media/Updated`.
    Webhook,
}

impl ScanTrigger {
    /// Every trigger, in a fixed order (the metric label set).
    pub const ALL: [Self; 5] = [
        Self::Api,
        Self::Schedule,
        Self::Startup,
        Self::Watcher,
        Self::Webhook,
    ];

    /// The trigger's name in logs, spans and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Schedule => "schedule",
            Self::Startup => "startup",
            Self::Watcher => "watcher",
            Self::Webhook => "webhook",
        }
    }

    /// The scan trigger of a scheduled-task run started by `trigger` (the
    /// task registry's `api` / `schedule` / `startup`). Anything else — a
    /// run with no recorded trigger — is the task's own schedule.
    #[must_use]
    pub fn from_task_trigger(trigger: &str) -> Self {
        match trigger {
            "api" => Self::Api,
            "startup" => Self::Startup,
            _ => Self::Schedule,
        }
    }
}

impl std::fmt::Display for ScanTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a queued refresh scan covers — the item-less stand-in for the
/// `Folder` a C# refresh validates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanTarget {
    /// Every library (`RootFolder.ValidateChildren`).
    All,
    /// One library, by its CollectionFolder id
    /// (`ProviderManager.RefreshCollectionFolderChildren`).
    Library(Uuid),
    /// The items at or under these filesystem paths — a folder's subtree
    /// (`Folder.ValidateChildren` on a series, season, album…). Rows under
    /// the paths whose files are gone are removed.
    Paths(Vec<String>),
    /// The paths the library monitor reports as changed — the disk watcher
    /// and the *arr webhooks (`FileRefresher.ProcessPathChanges`). Each path
    /// refreshes the nearest existing item at or above it
    /// (`BaseItem.ChangedExternally`, a `Default` refresh) and validates that
    /// item's subtree: new items are created, the rows whose files are gone
    /// are removed. The folders above that item are carried for context
    /// only — never refreshed — and no library-wide closing pass runs.
    ///
    /// ACCEPTED DIVERGENCE (approved by the owner on 2026-09-27): Ferrofin
    /// keeps a series' `DateLastMediaAdded` current on a watcher event — the
    /// closing folder aggregate pass writes that one column of a context
    /// series when a new episode moves it — where upstream updates it only
    /// on the series' own refresh (`MetadataService.cs:404-411`), i.e. at
    /// the next library scan.
    Changed(Vec<String>),
    /// The file items at these paths refreshing themselves
    /// (`ProviderManager.RefreshSingleItem`, a non-folder's `RefreshItem`):
    /// a movie's, an episode's, a track's refresh, or an Identify of one. A
    /// file that is gone is never removed by it — upstream's item refresh
    /// deletes nothing — and it runs no library-wide closing pass.
    Items(Vec<String>),
    /// `ProviderManager.RefreshArtist`: the CHILDREN of `folders` — the
    /// artist folders the artist's credited albums sit under — and the
    /// artist itself (its own row, when it has a folder at `path`). The items
    /// of `folders` themselves, other artists, are not refreshed.
    Artist {
        /// The artist's id.
        id: Uuid,
        /// The artist's own folder, when it is folder-backed.
        path: Option<String>,
        /// The artist folders whose children are validated.
        folders: Vec<String>,
    },
}

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

/// Whether an image type permits an item to hold more than one image of that
/// type — the only types whose ordering can be changed.
///
/// Port of `BaseItem.AllowsMultipleImages`: `true` for [`ImageType::Backdrop`]
/// and [`ImageType::Chapter`], `false` otherwise.
#[must_use]
pub fn image_type_allows_multiple(image_type: ImageType) -> bool {
    matches!(image_type, ImageType::Backdrop | ImageType::Chapter)
}

/// Applies a `/Years` sort order to the distinct year list.
///
/// Port of `_libraryManager.Sort(extractedItems, user, RequestHelpers.GetOrderBy(
/// sortBy, sortOrder))` in `YearsController.GetYears`, restricted to the keys a
/// `Year` item can actually be ordered by. Upstream sorts full `Year` entities,
/// whose orderable state is only their name/`SortName` (`"0000002020"`) and
/// `ProductionYear` — all three collapse to the numeric year — plus `Random`,
/// which C# implements as `OrderBy(_ => Guid.NewGuid())` and which is
/// reproduced here with the same construct.
///
/// An empty `order_by` is a no-op, exactly as upstream: `GetOrderBy` returns
/// `Array.Empty<...>()` for an absent `sortBy` and `LibraryManager.Sort` then
/// returns its input untouched. Any other key leaves the order alone rather
/// than inventing one — a `Year` carries no runtime, rating or play state to
/// sort on.
fn sort_years(years: &mut Vec<i32>, order_by: &[(ItemSortBy, SortOrder)]) {
    let Some((key, order)) = order_by.first().copied() else {
        return;
    };
    match key {
        ItemSortBy::SortName
        | ItemSortBy::Name
        | ItemSortBy::ProductionYear
        | ItemSortBy::PremiereDate
        | ItemSortBy::Default => {
            years.sort_unstable();
            if order == SortOrder::Descending {
                years.reverse();
            }
        }
        ItemSortBy::Random => {
            // C# `ItemSortBy.Random` => `OrderBy(i => Guid.NewGuid())`.
            let mut keyed: Vec<(Uuid, i32)> = years.iter().map(|y| (Uuid::new_v4(), *y)).collect();
            keyed.sort_unstable_by_key(|(k, _)| *k);
            if order == SortOrder::Descending {
                keyed.reverse();
            }
            *years = keyed.into_iter().map(|(_, y)| y).collect();
        }
        _ => {}
    }
}

/// Orchestrates the item library: queries, counts, people, genres, deletion.
///
/// Port of `ILibraryManager` (the object-safe, domain-tree-free subset). The
/// resolver/path/sort/named-view methods are intentionally omitted — they
/// depend on the un-ported C# `BaseItem` hierarchy and become `ferrofin-core`
/// free functions in Wave 6.
#[async_trait]
pub trait LibraryManager: Send + Sync {
    /// Gets a single item row by id, or `None` if it does not exist.
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError>;

    /// Resolves an item as a user, returning `None` when it is missing or hidden.
    /// A user-less request retains raw lookup semantics (for example API keys).
    /// Storage and policy evaluation failures remain errors, never invisibility.
    async fn get_item_by_id_for_user(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        let Some(item) = self.get_item_by_id(id).await? else {
            return Ok(None);
        };
        if let Some(user) = user
            && !self.is_item_visible_standalone(&item, user).await?
        {
            return Ok(None);
        }
        Ok(Some(item))
    }

    /// `BaseItem.ContainingFolderPath`: the folder a folder content-type
    /// override for `item` is keyed on. That is the item's own path for a
    /// folder, a disc rip that is no placeholder and a by-name item, a
    /// playlist file's directory, and the directory of its file (of the first
    /// part, for a stack) otherwise. `None` for an item without a path.
    ///
    /// The default knows only folders and files.
    async fn containing_folder_path(&self, item: &BaseItemEntity) -> Option<String> {
        let path = item.path.as_deref()?;
        if item.is_folder {
            return Some(path.to_owned());
        }
        std::path::Path::new(path)
            .parent()
            .and_then(std::path::Path::to_str)
            .map(str::to_owned)
    }

    /// `LibraryManager.GetInheritedContentType(item)`: the collection type of
    /// the library `item`'s top folder belongs to, else the configured type
    /// ([`Self::get_configured_content_type`]) of the parent farthest from
    /// `item` that has one.
    ///
    /// The default finds none, for a seam without libraries.
    async fn get_inherited_content_type(
        &self,
        _item: &BaseItemEntity,
    ) -> Result<Option<CollectionType>, ServiceError> {
        Ok(None)
    }

    /// `LibraryManager.GetConfiguredContentType(item)`: a library's own
    /// collection type for its `CollectionFolder`, else the folder
    /// content-type override (`ServerConfiguration.ContentTypes`) naming
    /// `item`'s [`containing folder`](Self::containing_folder_path) exactly.
    ///
    /// The default finds none, for a seam without a configuration.
    async fn get_configured_content_type(
        &self,
        _item: &BaseItemEntity,
    ) -> Result<Option<CollectionType>, ServiceError> {
        Ok(None)
    }

    /// The [`InternalPeopleQuery::access_filter`] for `user`: `None` when
    /// nothing is hidden from them (`PersonsController.BuildAccessFilter`).
    /// Managers must supply policy evaluation; the default fails closed.
    async fn build_access_filter(
        &self,
        _user: &UserEntity,
    ) -> Result<Option<InternalItemsQuery>, ServiceError> {
        Err(ServiceError::backend("access filtering is not configured"))
    }

    /// `BaseItem.IsVisible`: the item's own policy, used by parent browse gates.
    /// Managers must supply policy evaluation; the default fails closed.
    async fn is_item_visible(
        &self,
        _item: &BaseItemEntity,
        _user: &UserEntity,
    ) -> Result<bool, ServiceError> {
        Err(ServiceError::backend("item visibility is not configured"))
    }

    /// Applies the own-item IsVisible rule to a child set, preserving order.
    /// Concrete managers batch policy and hierarchy reads for Folder consumers.
    async fn filter_visible_items(
        &self,
        items: Vec<BaseItemEntity>,
        user: &UserEntity,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let mut visible = Vec::with_capacity(items.len());
        for item in items {
            if self.is_item_visible(&item, user).await? {
                visible.push(item);
            }
        }
        Ok(visible)
    }

    /// `IsVisibleStandalone`: item, ancestor and library access with kind overrides.
    /// Managers must supply policy evaluation; the default fails closed.
    async fn is_item_visible_standalone(
        &self,
        _item: &BaseItemEntity,
        _user: &UserEntity,
    ) -> Result<bool, ServiceError> {
        Err(ServiceError::backend(
            "standalone item visibility is not configured",
        ))
    }

    /// Returns existing, standalone-visible IDs in input order.
    /// Implementations can batch policy and hierarchy reads for queue checks.
    async fn get_visible_item_ids(
        &self,
        ids: &[Uuid],
        user: &UserEntity,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let mut visible = Vec::new();
        for &id in ids {
            if self
                .get_item_by_id_for_user(id, Some(user))
                .await?
                .is_some()
            {
                visible.push(id);
            }
        }
        Ok(visible)
    }

    /// Resolve the distinct physical owners whose extras an item presents.
    /// Concrete managers batch movie-version links and grouped-series folders.
    async fn get_extra_owner_ids_batch(
        &self,
        items: &[BaseItemEntity],
    ) -> Result<std::collections::HashMap<Uuid, Vec<Uuid>>, ServiceError> {
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = Uuid::parse_str(&item.id).ok()?;
                Some((id, vec![id]))
            })
            .collect())
    }

    /// Whether an item with `id` exists.
    ///
    /// Semantically `get_item_by_id(id).is_some()` — which is the default — but
    /// the concrete manager answers it with an existence probe rather than
    /// decoding the whole row, so the "item must exist" `404` gate on the image
    /// routes does not pay for a full `BaseItems` read it then discards.
    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        Ok(self.get_item_by_id(id).await?.is_some())
    }

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

    /// [`Self::get_item_images`] in the same read as whether they are
    /// auto-oriented when served; delegates to
    /// [`ItemRepository::get_image_infos_with_auto_orient`](crate::persistence::ItemRepository::get_image_infos_with_auto_orient).
    /// The default reads the images and orients nothing.
    ///
    /// # Errors
    ///
    /// [`ServiceError`] on a storage failure.
    async fn get_item_images_with_auto_orient(
        &self,
        item_id: Uuid,
    ) -> Result<(Vec<ItemImageInfo>, bool), ServiceError> {
        Ok((self.get_item_images(item_id).await?, false))
    }

    /// The image of the item's `index`-th chapter, if it has one.
    ///
    /// Chapter thumbnails are not item image rows — they live on the chapter
    /// itself (`Chapters.ImagePath`, written by the "Extract Chapter Images"
    /// task), so serving one resolves through the chapter, exactly as
    /// `BaseItem.GetImageInfo(ImageType.Chapter, index)` does upstream.
    /// Defaults to `None` for implementations without chapters.
    async fn get_chapter_image(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ItemImageInfo>, ServiceError> {
        let _ = (item_id, index);
        Ok(None)
    }

    /// Reorders an item's images by swapping the two `image_type` images at
    /// `index1` and `index2`.
    ///
    /// Port of `BaseItem.SwapImagesAsync` (the fan-in target of
    /// `ImageController.UpdateItemImageIndex`): only image types that permit
    /// multiple images can be reordered — Backdrop and Chapter, per C#
    /// `AllowsMultipleImages` — so any other type is a
    /// [`ServiceError::InvalidInput`] (the controller's `400`). An index that is
    /// out of range is a no-op, mirroring C#'s "nothing to do" branch. The
    /// concrete manager delegates to
    /// [`ItemRepository::swap_item_images`](crate::persistence::ItemRepository::swap_item_images).
    ///
    /// The default is a no-op so managers without a persistence seam (test
    /// doubles) compile unchanged; the concrete manager overrides it.
    ///
    /// # Errors
    ///
    /// [`ServiceError::InvalidInput`] when `image_type` does not allow multiple
    /// images, or [`ServiceError::Backend`] on a storage failure.
    ///
    /// Returns whether a swap happened, i.e. whether the caller owes the
    /// `ImageUpdate` repository save.
    async fn swap_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<bool, ServiceError> {
        if !image_type_allows_multiple(image_type) {
            return Err(ServiceError::invalid_input(
                "The change index operation is only applicable to backdrops and chapters",
            ));
        }
        let _ = (item_id, index1, index2);
        Ok(false)
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
    /// [`parent_id`]: ferrofin_db::entities::base_items::BaseItemEntity::parent_id
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
    /// [`BaseItemKind::UserRootFolder`] (directory + row at
    /// `DefaultUserViewsPath`) on first use; the concrete manager does the
    /// same through its root provisioner. This default resolves the single
    /// persisted `UserRootFolder` row (the first one, mirroring C#
    /// `FirstOrDefault`) via [`Self::get_item_list`] and reports `None` when
    /// absent — the behaviour of an implementation without a provisioner.
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

    /// Replaces an item's whole external-id set (`BaseItemProviders`) with
    /// `provider_ids`.
    ///
    /// The write behind C# `ItemUpdateController.UpdateItem`'s
    /// `item.ProviderIds = request.ProviderIds` (v10.11.8
    /// `Jellyfin.Api/Controllers/ItemUpdateController.cs:402-410`): an
    /// **assignment**, so a key the request omits is removed, not merged. The
    /// caller strips empty values first, exactly as the C# does.
    ///
    /// External ids live in their own table, not on [`BaseItemEntity`], so they
    /// cannot ride along with [`update_items`](Self::update_items) — hence a
    /// method of its own.
    ///
    /// The default is a no-op so the API-layer test doubles that implement this
    /// trait keep compiling; the real manager persists through the item
    /// repository. A double that asserts on this write overrides it.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the ids cannot be persisted.
    async fn update_item_provider_ids(
        &self,
        item_id: Uuid,
        provider_ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        let _ = (item_id, provider_ids);
        Ok(())
    }

    /// Saves only `item`'s rating: its `OfficialRating` and the parental score
    /// [`update_items`](Self::update_items) would re-derive from it, leaving
    /// the rest of the stored row as it is. A box set or playlist re-rated
    /// from its members (`BaseItem.UpdateRatingToItems`) saves through this,
    /// so the rest of a row it read earlier never overwrites a concurrent
    /// write — a share change to `Data` above all.
    ///
    /// The default is a no-op so the API-layer test doubles keep compiling;
    /// the real manager writes through the item repository.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the score cannot be derived or the row
    /// cannot be written.
    async fn update_item_rating(&self, item: BaseItemEntity) -> Result<(), ServiceError> {
        let _ = item;
        Ok(())
    }

    /// The locked metadata fields (upstream's `BaseItem.LockedFields`) of
    /// each of `item_ids`; an item with none is absent.
    ///
    /// The default knows of no locks, so the API-layer test doubles keep
    /// compiling; the real manager reads `BaseItemMetadataFields`.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the fields cannot be read.
    async fn get_locked_fields_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<MetadataField>>, ServiceError> {
        let _ = item_ids;
        Ok(std::collections::HashMap::new())
    }

    /// Replaces an item's locked-field set — the write behind
    /// `ItemUpdateController.UpdateItem`'s `item.LockedFields =
    /// request.LockedFields`. `field_ids` are `MetadataField` values as the
    /// client sent them, an unknown one included (upstream stores it too). Like the external ids, the set lives in its
    /// own table, so it cannot ride along with
    /// [`update_items`](Self::update_items).
    ///
    /// The default is a no-op (for test doubles).
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the set cannot be persisted.
    async fn update_item_locked_fields(
        &self,
        item_id: Uuid,
        field_ids: &[i32],
    ) -> Result<(), ServiceError> {
        let _ = (item_id, field_ids);
        Ok(())
    }

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
    /// modeled at this seam (Ferrofin tracks the version group solely by each row's
    /// `PrimaryVersionId` pointer); setting that pointer is the portable core of
    /// the merge.
    ///
    /// The default implementation reports the operation as unsupported, so a
    /// manager that does not persist version groups need not override it; the
    /// concrete `FerrofinLibraryManager` does.
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
    /// [`merge_versions`](Self::merge_versions)); `FerrofinLibraryManager` overrides
    /// it.
    async fn remove_alternate_sources(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let _ = item_id;
        Err(ServiceError::backend(
            "remove_alternate_sources not supported",
        ))
    }

    /// The other parts of a stacked video, ordered by `SortName`.
    ///
    /// Port of `Video.GetAdditionalParts(user)` (`Video.cs:492-498`): one
    /// `Video` item per path in the video's `AdditionalParts` (its id derived
    /// from the path as a `Video`'s), those that exist and — with a user —
    /// that the user may see (parental rating and tags). Empty for a video
    /// that is not stacked, and for any other item.
    ///
    /// The default implementation reports the operation as unsupported (see
    /// [`merge_versions`](Self::merge_versions)); `FerrofinLibraryManager`
    /// overrides it.
    async fn get_additional_parts(
        &self,
        video: &BaseItemEntity,
        user: Option<&UserEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let _ = (video, user);
        Err(ServiceError::backend("get_additional_parts not supported"))
    }

    /// Replaces `item`'s cast and crew with `people` when its kind credits
    /// any (`LibraryManager.UpdatePeople`, gated on `SupportsPeople`),
    /// creating the `Person` item each new name needs. The default writes
    /// nothing (stub and fake libraries).
    ///
    /// # Errors
    ///
    /// [`ServiceError`] on a storage failure.
    async fn update_people(
        &self,
        item: &BaseItemEntity,
        people: &[ferrofin_db::entities::base_items::PeopleEntity],
    ) -> Result<(), ServiceError> {
        let _ = (item, people);
        Ok(())
    }

    /// Gets the people rows attached to an item.
    async fn get_people(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError>;

    /// [`Self::get_people`] with the repository's total for the whole filtered
    /// set (C# `IPeopleRepository.GetPeople` returns a `QueryResult`). The
    /// default can only report the page it was handed; the concrete manager
    /// overrides it with the repository's count.
    async fn get_people_page(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<QueryResult<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        let people = self.get_people(query).await?;
        Ok(QueryResult::new(query.start_index, None, people))
    }

    /// Gets the credited people for a set of item ids at once, keyed by item —
    /// the batch form used to project a page of DTOs without a per-item
    /// `get_people`. The default loops the single-item form; the concrete manager
    /// overrides it.
    async fn get_people_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<
        std::collections::HashMap<Uuid, Vec<ferrofin_db::entities::base_items::PeopleEntity>>,
        ServiceError,
    > {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            let people = self
                .get_people(&InternalPeopleQuery {
                    item_id: id,
                    ..InternalPeopleQuery::default()
                })
                .await?;
            map.insert(id, people);
        }
        Ok(map)
    }

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

    /// The all-default by-name row C# stands in with when a slug lookup
    /// matches nothing — `item ??= new Genre()` in
    /// `GenresController.GetGenre` (identical on v10.11.8 and master, and the
    /// same line in `MusicGenresController`).
    ///
    /// It is why that route can never 404: the controller serializes a
    /// default-constructed entity, so the client gets a 200 whose `Id` is
    /// `Guid.Empty`, whose `LocationType` is `Virtual`, and which carries no
    /// `Name` or `Path` at all. Only the row `Type` distinguishes it.
    ///
    /// The default here cannot name the stored type (the kind → CLR-name table
    /// lives with the item repository), so the concrete manager overrides it.
    fn empty_by_name_item(&self, kind: BaseItemKind) -> BaseItemEntity {
        let _ = kind;
        BaseItemEntity {
            id: uuid::Uuid::nil().to_string().to_uppercase(),
            ..BaseItemEntity::default()
        }
    }

    /// Resolves the by-name item row of `kind` named `name` WITHOUT creating
    /// one when nothing matches.
    ///
    /// The non-materializing sibling of [`Self::get_named_item`]. C# splits the
    /// two the same way: `GetGenre`/`GetStudio` are `CreateItemByName<T>` and
    /// write, while `GenresController.GetItemFromSlugName` runs plain
    /// `GetItemList` queries and returns null on a miss — the slug branch must
    /// never mint a row for a name that was only a mis-spelling of a real one.
    async fn find_named_item(
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

    /// Resolves a single by-name item (genre, studio, artist, person, year, …)
    /// of the given [`BaseItemKind`] by its name, or `None` when no such row
    /// exists — **materializing** it when it does not, for the kinds Jellyfin
    /// materializes.
    ///
    /// Port of `ILibraryManager`'s by-name resolvers (`GetGenre`, `GetStudio`,
    /// `GetArtist`, `GetMusicGenre`, `GetYear`), which are all
    /// `CreateItemByName<T>`: they create the metadata folder and persist the
    /// row as a side effect of the lookup, which is why those routes never 404
    /// upstream. `GetPerson` is the exception — a plain lookup on both trees
    /// (`LibraryManager.cs:958-968` on v10.11.8, `:1195-1205` on master) — so
    /// `Person` is not in the provisioned set. Use [`Self::find_named_item`]
    /// where the lookup must not write.
    ///
    /// Matching is by cleaned name (Jellyfin's item-by-name id is derived from
    /// the name), delegating to [`Self::get_item_list`] filtered to `kind`; the
    /// first match wins, mirroring C# `FirstOrDefault`.
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
    /// Port of `ILibraryManager.GetPeopleItems` (Jellyfin 12.2): it sets
    /// [`must_have_item`](InternalPeopleQuery::must_have_item), fetches the
    /// credited people via [`Self::get_people_page`], then resolves each name to
    /// its `Person` [`BaseItemEntity`] (dropping any that no longer resolve, or
    /// that the access filter's user may not see), preserving the people
    /// query's paging. The total is the repository's, so
    /// it counts exactly the people a page can hand back. The default folds the
    /// calls so every impl gets it for free.
    async fn get_people_items(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        let query = InternalPeopleQuery {
            must_have_item: true,
            ..query.clone()
        };
        let page = self.get_people_page(&query).await?;
        let names: Vec<String> = page.items.into_iter().map(|p| p.name).collect();
        let items: Vec<BaseItemEntity> = self
            .get_named_items(BaseItemKind::Person, &names)
            .await?
            .into_iter()
            .flatten()
            .collect();
        // C# drops the people that fail `IsVisible(query.User)`. A person skips
        // the allowed-tags check and an item-by-name is never blocked as
        // unrated, so only a user with content restrictions — the one the
        // access filter exists for — can fail it.
        let items = match query.access_filter.as_ref().and_then(|f| f.user.as_ref()) {
            Some(user) => self.filter_visible_items(items, user).await?,
            None => items,
        };
        Ok(QueryResult::new(
            query.start_index,
            Some(page.total_record_count),
            items,
        ))
    }

    /// Batch form of [`Self::get_named_item`]: resolves each of `names` to its
    /// by-name item row of `kind`, returning one slot per input name in order
    /// (`None` where no row resolves), so callers can preserve their paging.
    ///
    /// The default loops [`Self::get_named_item`]; the concrete manager overrides
    /// it with a single `CleanName IN (…)` query — resolving a whole page of
    /// people/years in one round-trip instead of N.
    async fn get_named_items(
        &self,
        kind: BaseItemKind,
        names: &[String],
    ) -> Result<Vec<Option<BaseItemEntity>>, ServiceError> {
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            out.push(self.get_named_item(kind, name).await?);
        }
        Ok(out)
    }

    /// Id-only form of [`Self::get_named_items`]: resolves each of `names` to
    /// the id of its by-name item row of `kind`, one slot per input name in
    /// order (`None` where no row resolves).
    ///
    /// The DTO prefetch resolves a whole page's cast through this and reads
    /// nothing but the id, so the concrete manager overrides it with a
    /// two-column projection rather than materializing a full item row per
    /// credited name. The default delegates to [`Self::get_named_items`], so
    /// every implementation gets it for free with identical results.
    async fn get_named_item_ids(
        &self,
        kind: BaseItemKind,
        names: &[String],
    ) -> Result<Vec<Option<Uuid>>, ServiceError> {
        Ok(self
            .get_named_items(kind, names)
            .await?
            .into_iter()
            .map(|row| row.and_then(|r| Uuid::parse_str(&r.id).ok()))
            .collect())
    }

    /// Gets the library's production years, resolved to their by-name `Year`
    /// item rows, ordered by `query.order_by` and paged by
    /// `start_index`/`limit`.
    ///
    /// The reported total is the number of **distinct years**, captured before
    /// paging (C# `ibnItemsArray.Count`). Jellyfin only reaches that expression
    /// when `totalCount == -1`; with a user resolved it instead reports the
    /// out-param of `Folder.GetRecursiveChildren(user, query, out totalCount)`,
    /// which counts the underlying *media* items (559 for a 3-year fixture).
    /// That is an upstream bug — a paging total unrelated to the page — and is
    /// not ported; see `suite/parity/classifications.json`.
    ///
    /// Port of `YearsController.GetYears`: Jellyfin walks the (localized) item
    /// tree, collects each item's distinct `ProductionYear`, and resolves each
    /// through `GetYear`, which creates the `Year` item when it does not exist
    /// yet. Here the distinct years come from [`Self::get_distinct_years`]
    /// over the same `query` and are resolved via [`Self::get_named_items`];
    /// the concrete manager materializes a missing `Year` on that lookup, and
    /// the library scan creates every scanned year up front, so each year
    /// resolves. The default still drops a slot no implementation resolved
    /// (a fake without the provisioner), mirroring `.Where(i => i is not null)`.
    async fn get_years(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        let mut years = self.get_distinct_years(query).await?;
        years.retain(|y| *y > 0);
        // `.Distinct()` upstream; sorting first is what makes `dedup` total.
        //
        // Ascending is Ferrofin's RESTING order (no `sortBy`), and it is a
        // KNOWN DIVERGENCE, not parity — do not read the C# no-op as agreement.
        // Upstream's `GetAllItems` is
        // `items.Select(i => i.ProductionYear ?? 0).Where(i => i > 0).Distinct()`
        // (v10.11.8 YearsController.cs:220-227): LINQ `Distinct()` preserves
        // FIRST-OCCURRENCE order, so the resting order is the order years first
        // appear in `Folder.GetRecursiveChildren` — the in-memory folder walk.
        // `GetOrderBy` returning `Array.Empty` for an absent `sortBy` (and
        // `LibraryManager.Sort` then doing nothing) is what LEAVES that
        // enumeration order in place; it does not produce a sorted list.
        // Reproducing it would mean reproducing Jellyfin's BaseItem tree walk,
        // which Ferrofin does not have (see "There is no domain-object
        // hierarchy" in CLAUDE.md), so a deterministic ascending order is the
        // chosen behaviour and the divergence is recorded on the `GET /Years`
        // row of `suite/parity/classifications.json`. Every probe leg pins
        // `sortBy=SortName`, where the two agree exactly, including under paging.
        years.sort_unstable();
        years.dedup();
        sort_years(&mut years, &query.order_by);
        // C# captures `ibnItemsArray.Count` BEFORE `Skip`/`Take`, so the total
        // is the number of distinct years, not the size of the page.
        let total = i32::try_from(years.len()).unwrap_or(i32::MAX);
        let start = usize::try_from(query.start_index.unwrap_or(0).max(0)).unwrap_or(0);
        // Page the year list first, then resolve the slice in one query. Every
        // year resolves (the scan materializes them and the lookup creates any
        // straggler), so paging the names is paging the rows.
        let paged: Vec<String> = match query.limit.filter(|l| *l >= 0) {
            Some(limit) => years
                .into_iter()
                .skip(start)
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .map(|y| y.to_string())
                .collect(),
            None => years
                .into_iter()
                .skip(start)
                .map(|y| y.to_string())
                .collect(),
        };
        let items: Vec<BaseItemEntity> = self
            .get_named_items(BaseItemKind::Year, &paged)
            .await?
            .into_iter()
            .flatten()
            .collect();
        Ok(QueryResult::new(query.start_index, Some(total), items))
    }

    /// Gets aggregated legacy query-filter values for the matching items.
    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError>;

    /// Gets just the distinct production years of the matching items — the one
    /// facet [`Self::get_years`] uses.
    ///
    /// `/Years` used to read the whole legacy filter aggregate and keep only
    /// `.years`. That aggregate is four independent statements — distinct
    /// years, distinct official ratings, and a `MIN` over `ItemValues` for
    /// genres and for tags — so three of them were issued, run to completion,
    /// and dropped. Measured on the bench library that was 16.8 ms of the
    /// endpoint's 31.4 ms of SQL, and 3 of its 5 round trips.
    ///
    /// The default keeps the old behaviour for implementations that only have
    /// the aggregate (test fakes); the repository-backed one overrides it with
    /// the single `SELECT DISTINCT "ProductionYear"` statement.
    async fn get_distinct_years(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<i32>, ServiceError> {
        Ok(self.get_query_filters_legacy(query).await?.years)
    }

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

    /// Gets the distinct language codes for several stream types at once, keyed
    /// by type. The default loops [`Self::get_media_stream_languages`]; the
    /// concrete manager overrides it to resolve the item set once.
    async fn get_media_stream_languages_by_type(
        &self,
        stream_types: &[MediaStreamType],
        query: &InternalItemsQuery,
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(stream_types.len());
        for &t in stream_types {
            map.insert(t, self.get_media_stream_languages(t, query).await?);
        }
        Ok(map)
    }

    /// Queues a full library scan.
    async fn queue_library_scan(&self) -> Result<(), ServiceError>;

    /// Queues a full library scan, tagging the run's root span, its log lines
    /// and its metrics with why it was triggered ([`ScanTrigger`]). Defaults
    /// to the plain [`queue_library_scan`](Self::queue_library_scan) so
    /// existing implementations need no change; the real manager overrides it
    /// to record the `trigger`.
    async fn queue_library_scan_with_trigger(
        &self,
        _trigger: ScanTrigger,
    ) -> Result<(), ServiceError> {
        self.queue_library_scan().await
    }

    /// Queues a scan restricted to one library. `library_id` is the library's
    /// CollectionFolder id — what jellyfin-web's per-library "Scan Library"
    /// button refreshes via `POST /Items/{id}/Refresh`. Defaults to the full
    /// [`queue_library_scan`](Self::queue_library_scan) so existing
    /// implementations need no change; the real manager narrows the filesystem
    /// walk to that library's folders.
    async fn queue_library_scan_scoped(&self, _library_id: Uuid) -> Result<(), ServiceError> {
        self.queue_library_scan().await
    }

    /// Queues a scan of `target` whose items refresh with `options` — how
    /// `POST /Items/{itemId}/Refresh` refreshes anything with a path
    /// (`ProviderManager.RefreshItem`: a CollectionFolder refreshes its
    /// physical folders, any other folder validates its own children, both
    /// with the request's options, and a file item refreshes itself through
    /// the same metadata service — here the scan of its one path, with the
    /// scan's refresh decision, merge, probe, local readers and every
    /// metadata provider). The folders above a path-scoped target are not
    /// refreshed (their refresh modes are `None`), as upstream never
    /// refreshes a folder's ancestors. The other scan entry points refresh
    /// with the `MetadataRefreshOptions` constructor defaults.
    ///
    /// The default refuses: a manager without a scanner cannot honour the
    /// options, and widening the request into a default full scan would
    /// silently do something else.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] from a manager with no scanner, or whatever
    /// queueing the scan surfaces.
    async fn queue_refresh_scan(
        &self,
        target: ScanTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        let _ = (target, options);
        Err(ServiceError::backend(
            "queue_refresh_scan needs a library scanner, which this library manager has none of",
        ))
    }

    /// [`queue_refresh_scan`](Self::queue_refresh_scan), returning only once
    /// the refresh has run — the refresh `POST /Items/RemoteSearch/Apply/{id}`
    /// awaits (`ProviderManager.RefreshFullItem`). It goes into the scan
    /// queue's priority lane: ahead of every queued scan, and — while a scan
    /// runs — served inside that scan between two of its items, so it waits
    /// about one item, never a whole library scan. Returns `Ok(false)` when
    /// it was stopped before it finished (the server is shutting down).
    ///
    /// With `options.search_result` set, the items the scan covers are looked
    /// up by that chosen result (`MetadataService.ApplySearchResult`): their
    /// local readers are skipped and the result's provider ids, name and year
    /// pin the remote lookup, the fetcher it came from first. The item's own
    /// ids become the result's (`item.SetProviderIds`, the controller's half
    /// upstream) with the refresh's save of the item, so a refresh that fails
    /// leaves no half-applied ids.
    ///
    /// The default refuses, as [`queue_refresh_scan`](Self::queue_refresh_scan)'s does.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] from a manager with no scanner, or the scan
    /// failed.
    async fn run_refresh_scan(
        &self,
        target: ScanTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        let _ = (target, options);
        Err(ServiceError::backend(
            "run_refresh_scan needs a library scanner, which this library manager has none of",
        ))
    }

    /// Runs a full library scan and returns only when it has FINISHED.
    ///
    /// The scheduled-task entry point, and the one place the difference from
    /// `queue_library_scan` matters. Upstream's "Scan Media Library" task is
    /// `await ValidateMediaLibraryInternal(progress, ct)`
    /// (v10.11.8 `RefreshMediaLibraryTask.ExecuteAsync`), so the task stays
    /// `Running` for the whole scan and its `LastExecutionResult` records the
    /// real duration. A task that queued the scan and returned would report
    /// itself finished in 0 ms with the scan still writing — which is what the
    /// dashboard, and anything that waits on the task, would then believe.
    ///
    /// `trigger` is why the task ran (its schedule, its startup trigger, or
    /// the dashboard over the API); the scan's span, log lines and metrics
    /// carry it. `progress`, when given, receives the scan's progress as it
    /// runs (upstream's `IProgress<double>`: the items take 0–96 %, the
    /// closing passes 96–100 %).
    ///
    /// Returns `Ok(true)` once the scan ran to its end, and `Ok(false)` when
    /// it was stopped before it finished or refused because scanning has
    /// shut down — upstream's `OperationCanceledException`, which the task
    /// records as `Cancelled`.
    ///
    /// Defaults to the queueing form so implementations with no scanner need no
    /// change; the real manager overrides it.
    ///
    /// # Errors
    ///
    /// The scan failed (or panicked), so the task records `Failed`.
    async fn run_library_scan(
        &self,
        trigger: ScanTrigger,
        progress: Option<ScanProgressSink>,
    ) -> Result<bool, ServiceError> {
        let _ = (trigger, progress);
        self.queue_library_scan().await.map(|()| true)
    }

    /// Whether a full library validation is running: upstream's
    /// `ILibraryManager.IsScanRunning`, which `ValidateMediaLibraryInternal`
    /// holds for the whole of a full scan (the "Scan Media Library" task,
    /// `POST /Library/Refresh`), its closing passes included. The library
    /// monitor's path-scoped scans, one library's or one folder's refresh and
    /// the item refreshes never enter that method upstream, so they do not
    /// count.
    ///
    /// The database-heavy maintenance work stands down while it holds: the
    /// "Optimize database" and "Refresh People" tasks skip their run, and
    /// `POST /Backup/Create` refuses.
    ///
    /// A manager with no scanner runs no scan, so the default is `false`.
    fn is_scan_running(&self) -> bool {
        false
    }

    /// Stops scanning for good, as the host tears down: cancels the running
    /// scan (it stops before its next item, the one in progress written
    /// whole), drops the queued ones, refuses new ones, and returns once no
    /// scan is running — before the database it writes to is closed or a
    /// backup restore replaces its tree.
    ///
    /// A manager with no scanner runs no scan, so the default has nothing to
    /// stop.
    async fn shutdown_scans(&self) {}
}

fn _assert_object_safe_library_manager(_: &dyn LibraryManager) {}

/// The three playback permissions a user's policy imposes on a media source.
///
/// The inputs to `MediaSourceManager`'s per-user overwrite (v10.11.8
/// Emby.Server.Implementations/Library/MediaSourceManager.cs:204-217): an
/// AUDIO item's `SupportsTranscoding` becomes
/// `EnableAudioPlaybackTranscoding`; a VIDEO item's becomes
/// `EnableVideoPlaybackTranscoding`, and its `SupportsDirectStream` becomes
/// `EnablePlaybackRemuxing`. Everything else (photos, books, unknown media)
/// is left as the source built it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlaybackPermissions {
    /// `PermissionKind.EnableVideoPlaybackTranscoding`.
    pub video_transcoding: bool,
    /// `PermissionKind.EnableAudioPlaybackTranscoding`.
    pub audio_transcoding: bool,
    /// `PermissionKind.EnablePlaybackRemuxing`.
    pub remuxing: bool,
}

impl From<&UserPolicy> for PlaybackPermissions {
    fn from(policy: &UserPolicy) -> Self {
        Self {
            video_transcoding: policy.enable_video_playback_transcoding,
            audio_transcoding: policy.enable_audio_playback_transcoding,
            remuxing: policy.enable_playback_remuxing,
        }
    }
}

/// The user permissions `BaseItem.CanDelete(user)`, `CanDownload(user)` and
/// `GetPlayAccess(user)` read, from [`UserDataManager::get_content_permissions`].
///
/// Upstream reads each with `user.HasPermission(PermissionKind.…)` and the
/// folder list with `user.GetPreferenceValues<Guid>(PreferenceKind.
/// EnableContentDeletionFromFolders)` (`BaseItem.cs:854-879`,
/// `BoxSet.cs:107-110`, `Playlist.cs:273-276`, `BaseItem.cs:902-905`).
// Each flag is one independent `Permissions` row, not a state to model.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentPermissions {
    /// `PermissionKind.IsAdministrator` — deletes any playlist and any
    /// collection. It grants nothing for ordinary media: an administrator
    /// without "Allow media deletion" cannot delete a movie, as upstream.
    pub is_administrator: bool,
    /// `PermissionKind.EnableMediaPlayback` — controls `PlayAccess`, including
    /// for administrators, who have no override for this permission.
    pub enable_media_playback: bool,
    /// `PermissionKind.EnableContentDeletion` — the dashboard's "Allow media
    /// deletion" (any library).
    pub enable_content_deletion: bool,
    /// `PermissionKind.EnableContentDownloading`.
    pub enable_content_downloading: bool,
    /// `PermissionKind.EnableCollectionManagement` — deletes collections.
    pub enable_collection_management: bool,
    /// `PreferenceKind.EnableContentDeletionFromFolders` — the dashboard's
    /// "Allow media deletion from" list: the libraries (`CollectionFolder`
    /// ids) the user may delete from without the global permission. Values
    /// that are not GUIDs are dropped.
    pub content_deletion_folders: Vec<Uuid>,
}

impl PlaybackPermissions {
    /// Applies the overwrite to one media source, for an item of `media_type`.
    ///
    /// Port of the `if (user is not null)` block shared by
    /// `GetStaticMediaSources` (:355-372) and `GetPlaybackMediaSources`
    /// (:204-217) — the same three lines, on the static and the dynamic
    /// sources respectively. `media_type` is `BaseItem.MediaType` ("Audio" /
    /// "Video"); any other value is upstream's implicit `else`, which touches
    /// nothing.
    pub fn apply(
        self,
        media_type: Option<&str>,
        source: &mut ferrofin_model::dto::MediaSourceInfo,
    ) {
        match media_type {
            Some("Audio") => source.supports_transcoding = self.audio_transcoding,
            Some("Video") => {
                source.supports_transcoding = self.video_transcoding;
                source.supports_direct_stream = self.remuxing;
            }
            _ => {}
        }
    }
}

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

    /// The user write of `SessionManager.LogSessionActivity`
    /// (`SessionManager.cs:262-275`): stamps `user`'s `LastActivityDate` with
    /// `at`, never moving it backwards, and leaves the rest of the row as
    /// stored. Upstream saves the whole user (`UpdateUserAsync`) to change
    /// that one column.
    ///
    /// The default stamps nothing, so impls without a user store compile
    /// unchanged; the concrete manager overrides it.
    ///
    /// # Errors
    ///
    /// Whatever error the concrete write surfaces.
    async fn record_user_activity(
        &self,
        user: &UserEntity,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), ServiceError> {
        let _ = (user, at);
        Ok(())
    }

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
    /// [`UserConfiguration`] and
    /// [`UserPolicy`] from the `Users` row plus
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

    /// Clears a user's profile image: deletes the stored file (a missing file is
    /// not an error) and the `ImageInfos` row, as `DELETE /UserImage` does.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] when the file cannot be deleted for lack of
    /// permission (C# `UnauthorizedAccessException`); the row is then kept.
    async fn clear_profile_image(&self, user: &UserEntity) -> Result<(), ServiceError>;

    /// Stores caller-supplied profile-image bytes for a user.
    ///
    /// Port of the `POST /UserImage` tail: replace any existing profile-image row
    /// (like upstream, a previous file at a different path is left on disk), write
    /// the decoded bytes to `{UserConfigurationDirectoryPath}/{Username}/profile{extension}`,
    /// and persist the user (`_providerManager.SaveImage(stream, mime, path)` +
    /// `UpdateUserAsync`). `extension` is the image extension derived from the
    /// upload `Content-Type` (e.g. `.png`).
    ///
    /// The default implementation returns an error, so impls without a
    /// profile-image store compile unchanged; the concrete manager overrides it.
    ///
    /// # Errors
    ///
    /// [`ServiceError::InvalidInput`] when the username would escape the user
    /// configuration directory (C# `PathHelper.IsContainedIn` → `400 Invalid
    /// user.`), [`ServiceError::Backend`] from the default implementation, or
    /// whatever error the concrete write surfaces.
    async fn save_profile_image(
        &self,
        user: &UserEntity,
        content: &[u8],
        mime_type: &str,
        extension: &str,
    ) -> Result<(), ServiceError> {
        let _ = (user, content, mime_type, extension);
        Err(ServiceError::backend(
            "save_profile_image is not wired on this UserManager",
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
    /// [`ImageType::Profile`]: ferrofin_model::entities::ImageType::Profile
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

/// Observes a committed user-data write with its pinned save reason.
/// The NFO consumer subscribes weakly so it may retain the user-data manager.
#[async_trait]
pub trait UserDataSaveListener: Send + Sync {
    /// Handles a successfully persisted write. Listener failures must not undo it.
    async fn user_data_saved(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reason: ferrofin_model::entities::UserDataSaveReason,
    ) -> Result<(), ServiceError>;
}

/// The audio and subtitle tracks a playback progress report names, with the
/// user's "Remember audio selections" / "Remember subtitle selections"
/// settings, which decide what is kept of them (C#
/// `SessionManager.UpdatePlaybackSettings`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportedStreams {
    /// `PlaybackProgressInfo.AudioStreamIndex`.
    pub audio_stream_index: Option<i32>,
    /// `PlaybackProgressInfo.SubtitleStreamIndex`.
    pub subtitle_stream_index: Option<i32>,
    /// `User.RememberAudioSelections`.
    pub remember_audio: bool,
    /// `User.RememberSubtitleSelections`.
    pub remember_subtitles: bool,
}

impl ReportedStreams {
    /// Whether the report names a track of a kind the user does not want
    /// remembered, which resets both of the user's remembered tracks for the
    /// item (`OnPlaybackProgress`'s `ResetPlaybackStreamSelections` call,
    /// v12.2 `SessionManager.cs:1009-1013`).
    ///
    /// Deliberately as upstream: with one setting on and the other off, the
    /// reset also drops the index of the remembered kind that the same report
    /// just saved. Keeping it would diverge from v12.2, which is the owner's
    /// call (gl-116), not a fix to make here.
    #[must_use]
    pub fn resets_selections(&self) -> bool {
        (!self.remember_audio && self.audio_stream_index.is_some())
            || (!self.remember_subtitles && self.subtitle_stream_index.is_some())
    }

    /// Applies the report to the remembered `(audio, subtitle)` indices,
    /// returning whether either changed — `UpdatePlaybackSettings` (v12.2
    /// `SessionManager.cs:1016-1055`): with a setting on, a reported index
    /// replaces the remembered one and a report without one keeps it; with a
    /// setting off, the remembered index is cleared.
    pub fn apply_to(&self, audio: &mut Option<i32>, subtitle: &mut Option<i32>) -> bool {
        fn apply(remember: bool, reported: Option<i32>, kept: &mut Option<i32>) -> bool {
            let next = if remember { reported.or(*kept) } else { None };
            let changed = next != *kept;
            *kept = next;
            changed
        }
        let audio_changed = apply(self.remember_audio, self.audio_stream_index, audio);
        let subtitle_changed = apply(
            self.remember_subtitles,
            self.subtitle_stream_index,
            subtitle,
        );
        audio_changed || subtitle_changed
    }
}

/// Reads and writes per-user, per-item playback/rating data.
///
/// Port of `IUserDataManager`. User/item arguments become [`Uuid`] identities;
/// results are the [`UserItemDataDto`] presentation DTO. The committed-write
/// listener carries the source `UserDataSaved` reason to its NFO consumer.
#[async_trait]
pub trait UserDataManager: Send + Sync {
    /// Saves user data supplied as an update DTO.
    async fn save_user_data(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError>;

    /// Saves user data with the caller's source event reason.
    /// The default preserves compatibility with implementations without listeners.
    async fn save_user_data_with_reason(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        user_data: &UpdateUserItemDataDto,
        reason: ferrofin_model::entities::UserDataSaveReason,
    ) -> Result<(), ServiceError> {
        let _ = reason;
        self.save_user_data(user_id, item_id, user_data).await
    }

    /// Gets the remembered audio and subtitle stream indices for an item.
    ///
    /// Port of the `UserDataManager.GetUserData(user, item)` call inside
    /// `SetDefaultAudioAndSubtitleStreamIndices`: only the stream-index fields
    /// are needed, so this avoids constructing the full DTO.  Returns
    /// `(audio_stream_index, subtitle_stream_index)`, both `None` when no
    /// user-data row exists.
    async fn get_stream_default_data(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<(Option<i32>, Option<i32>), ServiceError> {
        let _ = (item_id, user_id);
        Ok((None, None))
    }

    /// Batch form of [`Self::get_stream_default_data`] for one user across
    /// many items, keyed by item id; an item that remembers neither index
    /// (or has no user-data row) is absent.
    ///
    /// Lets the item DTO's `MediaSources` select each page item's default
    /// streams (`GetStaticMediaSources(item, true, user)`, v12.2
    /// `DtoService.cs:410`) in one read instead of one per item. The default
    /// loops the single-item form; the concrete manager overrides it.
    async fn get_stream_default_data_batch(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, (Option<i32>, Option<i32>)>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &item_id in item_ids {
            let remembered = self.get_stream_default_data(item_id, user_id).await?;
            if remembered != (None, None) {
                map.insert(item_id, remembered);
            }
        }
        Ok(map)
    }

    /// Gets the presentation DTO of a user's data for an item, or `None`.
    async fn get_user_data_dto(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError>;

    /// Batch form of [`Self::get_user_data_dto`] for one user across many
    /// items, keyed by item id.
    ///
    /// The default loops the per-item call so impls compile unchanged; the
    /// concrete manager overrides it with a single query — the per-item form
    /// is an N+1 that dominates list-endpoint latency under concurrent load.
    async fn get_user_data_dtos(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &item_id in item_ids {
            if let Some(dto) = self.get_user_data_dto(item_id, user_id).await? {
                map.insert(item_id, dto);
            }
        }
        Ok(map)
    }

    /// Row-aware form of [`Self::get_user_data_dtos`].
    ///
    /// When an item has **no** stored `UserData` row, C#
    /// `UserDataManager.GetUserData(User, BaseItem)` synthesizes one keyed by
    /// `item.GetUserDataKeys()[0]` — `"Year-2020"` for a year, `"Studio-Acme"`
    /// for a studio, `"<series guid>001001"` for an episode. Deriving that needs
    /// the item's *metadata*, not just its id, so the id-only batch above can
    /// only ever answer with the guid.
    ///
    /// The DTO service already holds the rows it is projecting, so passing them
    /// through costs nothing and keeps the derivation off the N+1 path. The
    /// default delegates to the id-only form, leaving fakes and any impl that
    /// has not overridden it exactly as they were.
    ///
    /// `include_provider_ids` mirrors upstream's **navigation hydration**: the
    /// key of a movie/series/album is a provider id when the query that loaded
    /// the row ran `.Include(e => e.Provider)`, which
    /// `BaseItemRepository.ApplyNavigations` does only for
    /// `ItemFields.ProviderIds` (:442-445) while `RetrieveItem` always does
    /// (:825-829). Pass `false` and the derivation behaves as it does on a plain
    /// list query, which is what Jellyfin answers there.
    async fn get_user_data_dtos_for_rows(
        &self,
        items: &[BaseItemEntity],
        user_id: Uuid,
        include_provider_ids: bool,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let _ = include_provider_ids;
        let ids: Vec<Uuid> = items
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        self.get_user_data_dtos(&ids, user_id).await
    }

    /// Sets — or clears, when `likes` is `None` — a user's like flag for an item,
    /// returning the refreshed data DTO.
    ///
    /// Unlike [`Self::save_user_data`]'s merge semantics (an absent field is left
    /// unchanged), a `None` here **explicitly clears** the stored like, matching
    /// C# `UpdateUserItemRatingInternal` with `Likes = null`. The default
    /// implementation only persists the set case (via [`Self::save_user_data`]);
    /// the concrete manager overrides it to also persist a clear.
    async fn set_likes(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        likes: Option<bool>,
    ) -> Result<UserItemDataDto, ServiceError> {
        if let Some(v) = likes {
            let update = UpdateUserItemDataDto {
                likes: Some(v),
                ..UpdateUserItemDataDto::default()
            };
            self.save_user_data(user_id, item_id, &update).await?;
        }
        self.get_user_data_dto(item_id, user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user data for item {item_id}")))
    }

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

    /// Updates play state with PlaybackProgress or PlaybackFinished preserved.
    async fn update_play_state_with_reason(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
        reason: ferrofin_model::entities::UserDataSaveReason,
    ) -> Result<bool, ServiceError> {
        let _ = reason;
        self.update_play_state(user_id, item_id, reported_position_ticks)
            .await
    }

    /// Records a playback progress report for one user, returning whether the
    /// item is now played to completion — C# `SessionManager.OnPlaybackProgress
    /// (user, item, info)`: the reported position as [`Self::update_play_state`]
    /// takes it, the reported tracks as `UpdatePlaybackSettings` keeps them
    /// ([`ReportedStreams`]), one save for both (also when the report carries no
    /// position but changes a track), then
    /// [`Self::reset_playback_stream_selections`] when
    /// [`ReportedStreams::resets_selections`] holds.
    ///
    /// The default saves only the position before the reset, so fakes compile
    /// unchanged; the concrete manager keeps the tracks too.
    async fn update_play_progress(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
        streams: ReportedStreams,
    ) -> Result<bool, ServiceError> {
        let played = self
            .update_play_state(user_id, item_id, reported_position_ticks)
            .await?;
        if streams.resets_selections() {
            self.reset_playback_stream_selections(user_id, item_id)
                .await?;
        }
        Ok(played)
    }

    /// Records that playback of an item just started for a user.
    ///
    /// Port of the user-data half of C# `SessionManager.OnPlaybackStart`:
    /// increments `PlayCount`, stamps `LastPlayedDate` (the column Next Up's
    /// recently-watched filter reads — without this stamp a normally-watched
    /// series never surfaces there), and marks non-resumable kinds played
    /// outright.
    /// The default is a no-op so test fakes compile unchanged; the concrete
    /// manager implements the real write.
    async fn record_playback_start(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<(), ServiceError> {
        let _ = (user_id, item_id);
        Ok(())
    }

    /// Marks an item as played for a user, returning the refreshed data DTO.
    ///
    /// Port of `BaseItem.MarkPlayed`: sets `Played`, resets the resume position,
    /// stamps `LastPlayedDate` (defaulting to now), and — when `date_played` is
    /// supplied — increments `PlayCount` (always at least one). A container (a
    /// series, season, album, box set, playlist or folder) is not marked
    /// itself: the `Folder`/`BoxSet` overrides mark the items under it. The DTO
    /// is the container's user data, with its played state rolled up from
    /// those items where the kind takes it from its children.
    async fn mark_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError>;

    /// Marks an item as unplayed for a user, returning the refreshed data DTO.
    ///
    /// Port of `BaseItem.MarkUnplayed` / `ResetPlayedState`: clears `Played`,
    /// the play count, the resume position, and `LastPlayedDate`. A container
    /// resets the items under it instead, as [`Self::mark_played`] describes,
    /// and drops any play state left on its own row.
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

    /// The slice of the user's policy that `CanDelete(user)`, `GetPlayAccess(user)`
    /// and `CanDownload(user)` read (C# `BaseItem.IsAuthorizedToDelete`, its
    /// `BoxSet`/`Playlist` overrides, and `IsAuthorizedToDownload`), for the
    /// DTO builder's per-user `CanDelete`/`CanDownload`/`PlayAccess` and the delete
    /// endpoints' check, which use the same value.
    ///
    /// `None` means "no policy known". `CanDownload` then falls back to the
    /// file-level fact, while `CanDelete` is `false` and `PlayAccess` is `None`:
    /// neither permission is granted on an unknown policy. The default returns `None`; the
    /// concrete manager reads the `Permissions` and `Preferences` rows.
    async fn get_content_permissions(
        &self,
        user_id: Uuid,
    ) -> Result<Option<ContentPermissions>, ServiceError> {
        let _ = user_id;
        Ok(None)
    }

    /// The user's three PLAYBACK permissions.
    ///
    /// Backs `MediaSourceManager.GetStaticMediaSources` /
    /// `GetPlaybackMediaSources`' per-user overwrite (v10.11.8
    /// Emby.Server.Implementations/Library/MediaSourceManager.cs:355-372 and
    /// :204-217), which re-sets `SupportsTranscoding` — and, for video,
    /// `SupportsDirectStream` — from the user's policy AFTER the source has
    /// been built.
    ///
    /// It sits beside [`Self::get_content_permissions`] for the same reason
    /// that one does: both are a small bundle of `Permissions` rows read on one
    /// request for one caller, and both belong with the per-user data rather
    /// than in the manager that consumes them.
    ///
    /// `None` means "no policy known", and the caller must then leave the
    /// source untouched — upstream's `if (user is not null)` path. It is
    /// deliberately not three `false`s, which would tell a client the item can
    /// be neither remuxed nor transcoded. The default returns it; the concrete
    /// manager reads the `Permissions` rows.
    async fn get_playback_permissions(
        &self,
        user_id: Uuid,
    ) -> Result<Option<PlaybackPermissions>, ServiceError> {
        let _ = user_id;
        Ok(None)
    }
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

    /// Gets the top-level views including any the user hid from their home
    /// screen. Used by profile settings so hidden views can be re-enabled.
    /// Managers that do not implement hidden-view filtering can use the
    /// ordinary view list for both requests.
    async fn get_user_views_with_hidden(
        &self,
        user_id: Uuid,
        include_hidden: bool,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let _ = include_hidden;
        self.get_user_views(user_id).await
    }

    /// Gets the server's media folders — the user-root children.
    ///
    /// Port of `LibraryController.GetMediaFolders`, which returns
    /// `GetUserRootFolder().Children` sorted by `SortName`. Unlike
    /// [`get_user_views`](Self::get_user_views) (the library collection folders
    /// only), this also includes the auto-provisioned
    /// [`BaseItemKind`]`::ManualPlaylistsFolder`,
    /// provisioning it on first read if absent (Jellyfin lazily materializes it as
    /// a user-root child).
    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Gets the user's latest media, grouped by index container.
    ///
    /// Port of `UserViewManager.GetLatestItems`: ONE query across every parent
    /// (the user's views minus `latest_item_excludes`, or the `parent_id`
    /// folder), ordered `DateCreated DESC, SortName DESC, ProductionYear DESC`
    /// and over-fetched to `limit * 5` rows; a tvshows or music parent takes
    /// its own per-collection-type query instead
    /// ([`ItemRepository::get_latest_item_list`] — one Episode, Season or
    /// Series card per newest series for tvshows, the newest albums for music).
    /// The rows are then bucketed by their `LatestItemsIndexContainer` (episode
    /// → series, track → music album, photo → photo album; folders and every
    /// other kind — a `MusicAlbum` included — stand alone) in first-seen order,
    /// stopping once `limit` groups exist.
    ///
    /// Each tuple is the C# `Tuple<BaseItem, List<BaseItem>>`: the container
    /// (`None` for an ungrouped row) and the rows that fell under it. Virtual
    /// items are always excluded, matching upstream.
    ///
    /// [`ItemRepository::get_latest_item_list`]: crate::persistence::ItemRepository::get_latest_item_list
    async fn get_latest_items(
        &self,
        query: &crate::options::LatestItemsQuery,
        options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError>;

    /// The id of the Live TV `UserView` row every Live TV channel item is
    /// parented to, provisioning the view if it does not exist yet.
    ///
    /// Port of `LiveTvManager.GetInternalLiveTvFolder()` (v10.11.8
    /// src/Jellyfin.LiveTv/LiveTvManager.cs:1258-1262), which is
    /// `GetNamedView(name, CollectionType.livetv, name)` — and `GetNamedView`
    /// (LibraryManager.cs:2856-2898) CREATES the folder and its row on first
    /// read. `GuideManager.GetChannel` passes the result as every channel
    /// item's `ParentId`, so the guide refresh needs it before it can store a
    /// channel as an item.
    ///
    /// Unlike [`get_user_views`](Self::get_user_views) this has NO per-user
    /// Live TV gate: upstream's `GetInternalLiveTvFolder` takes no user, and
    /// the channel rows exist regardless of who may see them (visibility is
    /// decided by the query scope, not by whether the row was written).
    ///
    /// The default returns `None` — a service with no item store behind it
    /// cannot provision anything, and a caller must treat that as "no parent
    /// known", never as an error.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn get_internal_live_tv_folder_id(&self) -> Result<Option<Uuid>, ServiceError> {
        Ok(None)
    }
}

fn _assert_object_safe_user_view_manager(_: &dyn UserViewManager) {}

/// Resolves and opens playable media sources for an item.
///
/// Port of `IMediaSourceManager` (the API-facing subset). Streams and sources
/// surface as `ferrofin-model` DTOs at this layer; the `AddParts`/live-stream
/// direct-provider internals are dropped for v1.
#[async_trait]
pub trait MediaSourceManager: Send + Sync {
    /// Gets the media streams of an item as presentation DTOs.
    async fn get_media_streams(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaStream>, ServiceError>;

    /// Batch form of [`Self::get_media_streams`] for a whole page, keyed by item.
    ///
    /// Lets a list DTO projection load every item's streams in one query instead
    /// of an N+1 (the 2-connection SQLite pool makes query count the cost). The
    /// default loops the single-item form; the concrete manager overrides it.
    async fn get_media_streams_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<
        std::collections::HashMap<Uuid, Vec<ferrofin_model::entities_media::MediaStream>>,
        ServiceError,
    > {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            map.insert(id, self.get_media_streams(id).await?);
        }
        Ok(map)
    }

    /// The subset of `item_ids` that carry at least one LYRIC stream.
    ///
    /// Backs the DTO builder's `HasLyrics` (C# emits it on every `Audio` DTO,
    /// outside the `ItemFields` system — v10.11.8 DtoService.cs:308-311,
    /// `:421` on upstream master). The default
    /// derives it from [`Self::get_media_streams_batch`]; the concrete manager
    /// overrides it with a cheap ids-only query — same shape, and same reason
    /// for the override, as [`Self::get_item_ids_with_subtitles`].
    async fn get_item_ids_with_lyrics(&self, item_ids: &[Uuid]) -> Result<Vec<Uuid>, ServiceError> {
        let map = self.get_media_streams_batch(item_ids).await?;
        Ok(map
            .into_iter()
            .filter(|(_, streams)| {
                streams
                    .iter()
                    .any(|s| s.stream_type == ferrofin_model::entities::MediaStreamType::Lyric)
            })
            .map(|(id, _)| id)
            .collect())
    }

    /// The subset of `item_ids` that carry at least one subtitle stream.
    ///
    /// Backs the DTO builder's `HasSubtitles` (C# emits it on every video DTO,
    /// outside the `ItemFields` system, from the stored flag). The default
    /// derives it from [`Self::get_media_streams_batch`]; the concrete manager
    /// overrides it with a cheap ids-only `EXISTS` query so list pages don't
    /// materialize full stream rows.
    async fn get_item_ids_with_subtitles(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, ServiceError> {
        let map = self.get_media_streams_batch(item_ids).await?;
        Ok(map
            .into_iter()
            .filter(|(_, streams)| {
                streams
                    .iter()
                    .any(|s| s.stream_type == ferrofin_model::entities::MediaStreamType::Subtitle)
            })
            .map(|(id, _)| id)
            .collect())
    }

    /// The merged alternate-version rows for a page of primary item ids, keyed
    /// by primary id; primaries with no alternates are absent.
    ///
    /// Lets a DTO projection include every merged item's extra selectable
    /// sources (C# `GetStaticMediaSources` includes `LinkedAlternateVersions`)
    /// without a per-item query. The default reports no alternates — correct
    /// wherever version groups don't exist; the concrete manager overrides it
    /// with the repository's batched lookup.
    async fn get_alternate_versions_batch(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<
        std::collections::HashMap<Uuid, Vec<ferrofin_db::entities::base_items::BaseItemEntity>>,
        ServiceError,
    > {
        let _ = primary_ids;
        Ok(std::collections::HashMap::new())
    }

    /// [`Self::get_alternate_versions_batch`] with, beside each row, whether
    /// it has versions of its own (some other row names it as its
    /// `PrimaryVersionId`), read in the same query. The default flags every
    /// row the unflagged form returns.
    async fn get_alternate_versions_batch_flagged(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<
        std::collections::HashMap<
            Uuid,
            Vec<(ferrofin_db::entities::base_items::BaseItemEntity, bool)>,
        >,
        ServiceError,
    > {
        Ok(self
            .get_alternate_versions_batch(primary_ids)
            .await?
            .into_iter()
            .map(|(id, rows)| (id, rows.into_iter().map(|row| (row, true)).collect()))
            .collect())
    }

    /// Gets the media attachments of an item as presentation DTOs.
    async fn get_media_attachments(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaAttachment>, ServiceError>;

    /// Batch form of [`Self::get_media_attachments`] for a whole page, keyed by
    /// item (items with none are absent). The default loops the single-item
    /// form; the concrete manager runs one query.
    async fn get_media_attachments_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<
        std::collections::HashMap<Uuid, Vec<ferrofin_model::entities_media::MediaAttachment>>,
        ServiceError,
    > {
        let mut map = std::collections::HashMap::new();
        for &id in item_ids {
            let attachments = self.get_media_attachments(id).await?;
            if !attachments.is_empty() {
                map.insert(id, attachments);
            }
        }
        Ok(map)
    }

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

    /// Resolves the media source a stream request reads, for direct play and
    /// transcoding alike.
    ///
    /// Port of the selection in `StreamingHelpers.GetStreamingState`: an open
    /// live stream (`live_stream_id`) wins outright, since its source is the
    /// buffered copy of the tuner and the static sources would dial it again.
    /// Otherwise the item's static sources (unsubstituted: the server reads
    /// the file itself), taking the one `media_source_id` names, the first when
    /// none is named, and the first when the named id is the item's own. An id
    /// naming no source is `NotFound`, a deliberate divergence: upstream fails
    /// the request by accident, `400` for an unknown GUID (a null-source
    /// `ArgumentNullException`) and `500` for a non-GUID (`Guid.Parse`).
    ///
    /// TODO: two upstream steps are not ported. Upstream picks from
    /// `GetPlaybackMediaSources`, whose list has symlinked paths resolved and is
    /// ordered by `SortMediaSources` (placeholders dropped, widest first after
    /// the preferred version). It also first reuses the source of the
    /// `PlaySessionId`'s running transcode job. Fix path: select from
    /// [`Self::get_playback_media_sources`] once it can be called without a
    /// user, and look the job up through the transcode manager.
    async fn get_stream_media_source(
        &self,
        item_id: Uuid,
        media_source_id: Option<&str>,
        live_stream_id: Option<&str>,
    ) -> Result<MediaSourceInfo, ServiceError> {
        if let Some(live_stream_id) = live_stream_id.map(str::trim).filter(|id| !id.is_empty()) {
            return self.get_live_stream(live_stream_id).await;
        }
        let sources = self.get_static_media_sources(item_id, false, None).await?;
        let chosen = match media_source_id.filter(|id| !id.is_empty()) {
            Some(id) => match sources.iter().position(|s| s.id_matches(id)) {
                Some(i) => sources.into_iter().nth(i),
                None if Uuid::parse_str(id).is_ok_and(|g| g == item_id) => {
                    sources.into_iter().next()
                }
                None => None,
            },
            None => sources.into_iter().next(),
        };
        chosen.ok_or_else(|| ServiceError::NotFound(format!("no media source for item {item_id}")))
    }

    /// Closes an open live stream.
    async fn close_live_stream(&self, id: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_media_source_manager(_: &dyn MediaSourceManager) {}

/// Orchestrates search across registered providers.
///
/// Port of `ISearchManager`. The `AddParts`/`GetProviders` provider-registry
/// methods are dropped (registration is `ferrofin-core`'s job); results reuse
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

    /// Signals that `path` changed on disk, as the filesystem watcher saw it
    /// (a refresh it starts is labelled [`ScanTrigger::Watcher`]).
    async fn report_file_system_changed(&self, path: &str) -> Result<(), ServiceError>;

    /// Signals that `path` changed on disk, as an *arr webhook reported it
    /// (`POST /Library/{Series,Movies}/…`, `POST /Library/Media/Updated`):
    /// upstream's same `ReportFileSystemChanged` — the same debounce and the
    /// same refresh as the watcher's — except that a refresh it starts is
    /// labelled [`ScanTrigger::Webhook`] in logs and metrics. Defaults to
    /// [`report_file_system_changed`](Self::report_file_system_changed).
    async fn report_webhook_change(&self, path: &str) -> Result<(), ServiceError> {
        self.report_file_system_changed(path).await
    }
}

fn _assert_object_safe_library_monitor(_: &dyn LibraryMonitor) {}

/// The server's own file writes, which the library monitor must not take for
/// a change to rescan: `ILibraryMonitor.ReportFileSystemChangeBeginning` and
/// `ReportFileSystemChangeComplete` as upstream's writers call them around a
/// subtitle, lyric, NFO or image save, and around a collection's or a
/// playlist's folder. Synchronous, so a blocking write can bracket itself; a
/// [`SelfWrite`] span ends a write on every way out, as upstream's `finally`.
pub trait SelfWriteGuard: Send + Sync {
    /// The server starts writing `path`: a change to it, under it, or to the
    /// folder holding it is ignored until it completes.
    fn begin(&self, path: &str);

    /// The write to `path` is over. The change stays ignored a while longer,
    /// since filesystem events often arrive late; with `refresh_path` the path
    /// is then reported as changed.
    fn complete(&self, path: &str, refresh_path: bool);
}

fn _assert_object_safe_self_write_guard(_: &dyn SelfWriteGuard) {}

/// A [`SelfWriteGuard`] the composition root fills once the library monitor
/// exists. The writers are built before the monitor, so they hold this slot;
/// a write before it is filled is not bracketed, and the monitor watches
/// nothing yet then.
#[derive(Default)]
pub struct SelfWriteSlot {
    guard: std::sync::OnceLock<std::sync::Arc<dyn SelfWriteGuard>>,
}

impl std::fmt::Debug for SelfWriteSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfWriteSlot")
            .field("filled", &self.guard.get().is_some())
            .finish_non_exhaustive()
    }
}

impl SelfWriteSlot {
    /// Fills the slot with the monitor's guard. The composition root fills
    /// it once; a second call is ignored (and asserted against in debug
    /// builds, as a wiring mistake).
    pub fn set(&self, guard: std::sync::Arc<dyn SelfWriteGuard>) {
        let first = self.guard.set(guard).is_ok();
        debug_assert!(first, "SelfWriteSlot filled twice");
    }
}

impl SelfWriteGuard for SelfWriteSlot {
    fn begin(&self, path: &str) {
        if let Some(guard) = self.guard.get() {
            guard.begin(path);
        }
    }

    fn complete(&self, path: &str, refresh_path: bool) {
        if let Some(guard) = self.guard.get() {
            guard.complete(path, refresh_path);
        }
    }
}

/// One server write under `path`: begun when created, completed (without a
/// refresh, as every upstream writer does) when dropped, so an early return
/// or an error still ends it. With no guard it does nothing.
#[must_use = "the write is complete as soon as this is dropped"]
pub struct SelfWrite {
    guard: Option<std::sync::Arc<dyn SelfWriteGuard>>,
    path: String,
}

impl std::fmt::Debug for SelfWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfWrite")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SelfWrite {
    /// Begins the write of `path` on `guard`, when there is one.
    pub fn begin(
        guard: Option<&std::sync::Arc<dyn SelfWriteGuard>>,
        path: impl Into<String>,
    ) -> Self {
        let path = path.into();
        let guard = guard.cloned();
        if let Some(guard) = &guard {
            guard.begin(&path);
        }
        Self { guard, path }
    }
}

impl Drop for SelfWrite {
    fn drop(&mut self) {
        if let Some(guard) = &self.guard {
            guard.complete(&self.path, false);
        }
    }
}

/// Manages the on-disk **virtual-folder** tree that backs the library-structure
/// admin routes (`/Library/VirtualFolders*`, `/Library/PhysicalPaths`).
///
/// Port of the virtual-folder surface of `ILibraryManager`
/// (`GetVirtualFolders`, `AddVirtualFolder`, `RemoveVirtualFolder`,
/// `AddMediaPath`, `UpdateMediaPath`, `RemoveMediaPath`) plus the rename +
/// library-option-update flows the `LibraryStructureController` drives directly.
///
/// In Jellyfin a "virtual folder" is a directory under
/// `ApplicationPaths.DefaultUserViewsPath`; each holds `.mblink` shortcut files
/// (one per media path, containing the target path as plain text), an optional
/// `<type>.collection` marker file, and an `options.xml` carrying the serialized
/// [`LibraryOptions`]. This trait models exactly that filesystem contract — it
/// is independent of the DB-backed item tree, so a concrete impl only needs the
/// root user-views path and is fully testable over a temp directory.
///
/// Every caller-supplied library name must denote one directory directly under
/// that root, as upstream's `FileSystemHelper.GetChildPath` enforces: a blank
/// name, `.`/`..`, an absolute path or anything with a separator never reaches
/// the filesystem. A blank name to remove or rename is a
/// [`ServiceError::InvalidInput`], as is a rename's new name that is not such a
/// folder (checked before the current name); any other such name is a
/// [`ServiceError::NotFound`] for the library being looked up. Adding a library
/// named `.` or `..` de-duplicates it to `.2`/`..2`, as upstream does.
///
/// The `refresh_library` flag and the C# `ILibraryMonitor` stop/start dance are
/// dropped: the scan pipeline and filesystem watcher are later-wave subsystems,
/// so mutations take effect on disk immediately and the caller-requested refresh
/// is a documented no-op at this seam (matching how `queue_library_scan` is a
/// no-op today).
#[async_trait]
pub trait VirtualFolderManager: Send + Sync {
    /// Lists every configured virtual folder.
    ///
    /// Port of `ILibraryManager.GetVirtualFolders`: each directory under the
    /// user-views root becomes a [`VirtualFolderInfo`] whose `Locations` are the
    /// resolved `.mblink` shortcut targets (sorted), `CollectionType` comes from
    /// the `<type>.collection` marker, and `LibraryOptions` from `options.xml`.
    /// The `ItemId`/`PrimaryImageItemId`/refresh-state fields depend on the
    /// DB-backed collection-folder rows and refresh queue, which are absent at
    /// this seam, so they are left unset (Jellyfin leaves them null too when the
    /// folder has not yet been materialized as an item).
    async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError>;

    /// The configured libraries' names, item ids, locations, collection types
    /// and options, for per-item and per-request lookups.
    ///
    /// Upstream reads these from memory: `LibraryManager.GetLibraryOptions(item)`
    /// returns the `CollectionFolder`'s cached options, refreshed when they are
    /// saved, and `CollectionFolder.CollectionType` is an item property. An
    /// implementation may therefore serve a snapshot, as long as every change
    /// made through this manager is visible to the next call. The refresh
    /// state fields are not meaningful here; use
    /// [`get_virtual_folders`](Self::get_virtual_folders) for the dashboard.
    ///
    /// The default lists the folders afresh.
    async fn library_folders(&self) -> Result<std::sync::Arc<[VirtualFolderInfo]>, ServiceError> {
        Ok(self.get_virtual_folders().await?.into())
    }

    /// Adds a virtual folder with the given name, optional collection type,
    /// media paths, and library options.
    ///
    /// Port of `ILibraryManager.AddVirtualFolder`: the name is sanitized and
    /// de-duplicated (a numeric suffix is appended when the directory already
    /// exists), each media path must exist on disk (else
    /// [`ServiceError::InvalidInput`]), the collection marker + `options.xml` are
    /// written, and one `.mblink` shortcut is created per media path.
    ///
    /// Returns the created library's `CollectionFolder` id (`None` when its
    /// folder projects none). A caller acting on the new library must use it,
    /// not `name`: sanitizing and de-duplicating can store it under another
    /// name, possibly an existing library's.
    async fn add_virtual_folder(
        &self,
        name: &str,
        collection_type: Option<CollectionTypeOptions>,
        options: &LibraryOptions,
    ) -> Result<Option<Uuid>, ServiceError>;

    /// Removes the named virtual folder (its whole directory).
    ///
    /// Port of `ILibraryManager.RemoveVirtualFolder`: a missing folder is a
    /// [`ServiceError::NotFound`] (mirroring the C# `FileNotFoundException` the
    /// controller maps to `404`).
    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError>;

    /// Renames a virtual folder from `name` to `new_name`.
    ///
    /// Port of `LibraryStructureController.RenameVirtualFolder`: the source must
    /// exist ([`ServiceError::NotFound`] otherwise) and — unless the rename is a
    /// pure case change of the same path — the target must not already exist
    /// ([`ServiceError::Conflict`] otherwise). A case-only rename goes via a
    /// temporary directory, matching the C# case-insensitive handling.
    async fn rename_virtual_folder(&self, name: &str, new_name: &str) -> Result<(), ServiceError>;

    /// Removes every library row whose directory no longer exists, returning how
    /// many were removed.
    ///
    /// Port of the tail of `LibraryManager.ValidateTopLibraryFolders`: the pass
    /// that deletes a `CollectionFolder` child of the user root when
    /// `!Directory.Exists(collectionFolder.Path)`. Upstream runs it after every
    /// structural change and at the start of a library validation; implementations
    /// here call it from the mutators and the scanner does so at scan start, so a
    /// library directory that disappears behind the API's back stops haunting
    /// `/UserViews`.
    ///
    /// The default is a no-op, for seams with no item store attached.
    async fn prune_orphan_collection_folders(&self) -> Result<usize, ServiceError> {
        Ok(0)
    }

    /// Adds a media path (and its `.mblink` shortcut) to an existing library.
    ///
    /// Port of `ILibraryManager.AddMediaPath`: the library must exist and the
    /// path must exist on disk; the shortcut is created and the path is appended
    /// to the library's `options.xml` `PathInfos`.
    async fn add_media_path(
        &self,
        virtual_folder_name: &str,
        path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError>;

    /// Updates a media path's options within a library.
    ///
    /// Port of `ILibraryManager.UpdateMediaPath`: replaces the matching
    /// `PathInfos` entry (by path) in the library's `options.xml`. The library
    /// must exist.
    async fn update_media_path(
        &self,
        virtual_folder_name: &str,
        path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError>;

    /// Removes a media path (and its `.mblink` shortcut) from a library.
    ///
    /// Port of `ILibraryManager.RemoveMediaPath`: deletes the shortcut that
    /// resolves to `path` and drops the matching `PathInfos` entry from
    /// `options.xml`. The library must exist ([`ServiceError::NotFound`]
    /// otherwise).
    async fn remove_media_path(
        &self,
        virtual_folder_name: &str,
        path: &str,
    ) -> Result<(), ServiceError>;

    /// Replaces a library's options wholesale.
    ///
    /// Port of the `LibraryStructureController.UpdateLibraryOptions` tail
    /// (`CollectionFolder.UpdateLibraryOptions`): looks the library up by its
    /// item id, creates a shortcut for any newly-referenced media path, then
    /// persists the supplied [`LibraryOptions`] to `options.xml`. A library id
    /// that resolves to no folder is a [`ServiceError::NotFound`].
    ///
    /// The lookup is by name here: at this filesystem seam the DB item-id of a
    /// collection folder is not modeled, so the caller resolves the id to the
    /// folder name (its directory name) first. Passing the folder name keeps the
    /// operation self-contained.
    async fn update_library_options(
        &self,
        virtual_folder_name: &str,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError>;

    /// Lists the physical (resolved) locations across every virtual folder.
    ///
    /// Port of `LibraryController.GetPhysicalPaths`
    /// (`RootFolder.Children.SelectMany(c => c.PhysicalLocations)`): the union of
    /// every virtual folder's resolved `.mblink` targets.
    async fn get_physical_paths(&self) -> Result<Vec<String>, ServiceError> {
        let mut paths = Vec::new();
        for folder in self.get_virtual_folders().await? {
            paths.extend(folder.locations);
        }
        Ok(paths)
    }
}

fn _assert_object_safe_virtual_folder_manager(_: &dyn VirtualFolderManager) {}

/// A reference to a similar item by external provider id, as a remote
/// similarity provider returns it — port of
/// `MediaBrowser.Controller.Library.SimilarItemReference`.
///
/// A remote provider knows nothing about the local library; the manager resolves
/// each reference to a library item by looking the id up in `BaseItemProviders`.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarItemReference {
    /// The provider-id key the value is stored under (`Tmdb`,
    /// `MusicBrainzArtist`, …).
    pub provider_name: String,
    /// The provider-id value.
    pub provider_id: String,
    /// The provider's own similarity score, `0.0`–`1.0`. `None` lets the
    /// manager derive one from the reference's position in the result list.
    pub score: Option<f32>,
}

/// One page of a remote similarity provider's references — what one step of
/// the C# provider's `IAsyncEnumerable` walk yields (a TMDB results page).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimilarItemsPage {
    /// The page's references, in the provider's order. Empty when the walk is
    /// over: no such page, the seed has no id the provider is keyed by, or the
    /// source failed.
    pub references: Vec<SimilarItemReference>,
    /// How many pages the source reports in all (TMDB's `total_pages`); the
    /// manager asks for the next page only below it.
    pub total_pages: i32,
}

/// The query options a similarity provider is handed — port of
/// `MediaBrowser.Controller.Library.SimilarItemsQuery`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimilarItemsQuery {
    /// The requesting user, when the request is user-scoped.
    pub user_id: Option<Uuid>,
    /// How many results are still wanted.
    pub limit: Option<i32>,
    /// Item ids already accounted for, which must not be returned again.
    pub exclude_item_ids: Vec<Uuid>,
    /// Artist ids to exclude (an artist's own catalog).
    pub exclude_artist_ids: Vec<Uuid>,
}

/// A similarity provider backed by an external service — port of
/// `IRemoteSimilarItemsProvider`.
///
/// Local similarity is not a trait: Ferrofin has exactly one local scorer (the
/// weighted genre/tag/people overlap query, upstream's six identically-named
/// `"Local Genre/Tag"` providers collapsed into one), so it stays inline in the
/// manager rather than behind a one-implementation seam.
#[async_trait]
pub trait RemoteSimilarItemsProvider: Send + Sync {
    /// The provider's display name — the string a library's
    /// `TypeOptions.SimilarItemProviders` lists to enable it.
    fn name(&self) -> &str;

    /// Whether this provider serves `item_kind`.
    fn supports(&self, item_kind: BaseItemKind) -> bool;

    /// How long the manager may reuse this provider's results from disk.
    /// `None` disables caching for it.
    fn cache_duration(&self) -> Option<std::time::Duration> {
        None
    }

    /// Page `page` (1-based) of the similar-item references for `seed`.
    ///
    /// Upstream's provider is an `IAsyncEnumerable` that pages its source as
    /// it is consumed, so the manager's early stop also stops the paging. Here
    /// the manager drives the walk itself: page 1, then each next page while
    /// the last one was not empty, is below its `total_pages`, and the manager
    /// still wants references. A source with no pages answers everything on
    /// page 1 and reports one page.
    ///
    /// `seed_provider_ids` are the seed's external ids (`BaseItemProviders`),
    /// resolved by the manager: a remote provider is keyed by one of them and
    /// has no repository of its own. C# reads them off the item, which carries
    /// its `ProviderIds` dictionary in memory.
    ///
    /// Best-effort: a provider that fails returns an empty page rather than an
    /// error, matching the C# provider's `yield break` on a failed request.
    async fn similar_items_page(
        &self,
        seed: &BaseItemEntity,
        seed_provider_ids: &std::collections::HashMap<String, String>,
        query: &SimilarItemsQuery,
        page: i32,
    ) -> SimilarItemsPage;
}

fn _assert_object_safe_remote_similar_items_provider(_: &dyn RemoteSimilarItemsProvider) {}

/// Finds items similar to a seed and builds recommendation categories.
///
/// Port of `ISimilarItemsManager`. The generic `GetSimilarItemsProviders<T>` and
/// `AddParts` registry methods are dropped; similar-item results become
/// [`BaseItemEntity`] rows and recommendations [`SimilarItemsRecommendation`].
#[async_trait]
pub trait SimilarItemsManager: Send + Sync {
    /// Gets items similar to `item_id`.
    ///
    /// `Ok(None)` means the seed does not exist — C#
    /// `LibraryController.GetSimilarItems` answers `404` for it
    /// (`if (item is null) { return NotFound(); }`), which an empty `Vec` could
    /// not be told apart from. `Ok(Some(vec![]))` is the legitimate empty
    /// result: the controller's `Episode`/by-name short-circuit, or a seed
    /// nothing resembles.
    async fn get_similar_items(
        &self,
        item_id: Uuid,
        exclude_artist_ids: &[Uuid],
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
        limit: Option<i32>,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError>;

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

    use super::{
        ItemSortBy, SearchResult, SimilarItemsRecommendation, SortOrder,
        image_type_allows_multiple, sort_years,
    };
    use ferrofin_model::dto::RecommendationType;
    use ferrofin_model::entities::ImageType;
    use std::sync::Arc;

    use super::{SelfWrite, SelfWriteGuard, SelfWriteSlot};

    /// A [`SelfWriteGuard`] recording each call, in order.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<String>>);

    impl SelfWriteGuard for Recording {
        fn begin(&self, path: &str) {
            self.0.lock().unwrap().push(format!("begin {path}"));
        }
        fn complete(&self, path: &str, refresh_path: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete {path} {refresh_path}"));
        }
    }

    impl Recording {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A [`SelfWrite`] begins when made and completes, without a refresh,
    /// when dropped; the slot forwards only once filled, and no guard is no
    /// bracketing.
    #[test]
    fn a_self_write_completes_when_dropped_and_the_slot_forwards_once_filled() {
        let slot = Arc::new(SelfWriteSlot::default());
        let guard: Arc<dyn SelfWriteGuard> = slot.clone();
        drop(SelfWrite::begin(Some(&guard), "/media/a.srt"));
        let recording = Arc::new(Recording::default());
        slot.set(recording.clone());
        {
            let _write = SelfWrite::begin(Some(&guard), "/media/b.srt");
            assert_eq!(recording.calls(), ["begin /media/b.srt"]);
        }
        drop(SelfWrite::begin(None, "/media/c.srt"));
        assert_eq!(
            recording.calls(),
            ["begin /media/b.srt", "complete /media/b.srt false"]
        );
    }

    #[test]
    fn only_backdrop_and_chapter_allow_multiple_images() {
        assert!(image_type_allows_multiple(ImageType::Backdrop));
        assert!(image_type_allows_multiple(ImageType::Chapter));
        for other in [
            ImageType::Primary,
            ImageType::Art,
            ImageType::Banner,
            ImageType::Logo,
            ImageType::Thumb,
            ImageType::Disc,
            ImageType::Box,
            ImageType::Screenshot,
            ImageType::Menu,
            ImageType::BoxRear,
            ImageType::Profile,
        ] {
            assert!(!image_type_allows_multiple(other), "{other:?}");
        }
    }

    /// `UpdatePlaybackSettings` and the reset condition (v12.2
    /// `SessionManager.cs:1009-1055`, jellyfin#16769 and #17044), for one kind
    /// (audio): `(remember, reported, stored) → (kept, changed, resets)`.
    #[test]
    fn reported_streams_keep_and_reset_as_upstream() {
        use super::ReportedStreams;
        for (remember, reported, stored, kept, changed, resets) in [
            // On: a new index is saved, the same one is no change, none keeps it.
            (true, Some(2), Some(1), Some(2), true, false),
            (true, Some(1), Some(1), Some(1), false, false),
            (true, None, Some(1), Some(1), false, false),
            (true, Some(2), None, Some(2), true, false),
            // Off: the stored index is cleared; a reported one also resets.
            (false, None, Some(1), None, true, false),
            (false, None, None, None, false, false),
            (false, Some(2), Some(1), None, true, true),
            (false, Some(2), None, None, false, true),
        ] {
            let streams = ReportedStreams {
                audio_stream_index: reported,
                subtitle_stream_index: None,
                remember_audio: remember,
                remember_subtitles: true,
            };
            let (mut audio, mut subtitle) = (stored, None);
            let case = format!("{remember} {reported:?} {stored:?}");
            assert_eq!(
                streams.apply_to(&mut audio, &mut subtitle),
                changed,
                "{case}"
            );
            assert_eq!(audio, kept, "{case}");
            assert_eq!(subtitle, None, "{case}");
            assert_eq!(streams.resets_selections(), resets, "{case}");
        }

        // Mixed: audio remembered, subtitles not, and the report names both.
        // The audio index is kept by `apply_to` and the subtitle one cleared,
        // and the report still resets both afterwards, as upstream.
        let mixed = ReportedStreams {
            audio_stream_index: Some(2),
            subtitle_stream_index: Some(4),
            remember_audio: true,
            remember_subtitles: false,
        };
        let (mut audio, mut subtitle) = (Some(1), Some(3));
        assert!(mixed.apply_to(&mut audio, &mut subtitle));
        assert_eq!((audio, subtitle), (Some(2), None));
        assert!(mixed.resets_selections());
    }

    #[test]
    fn sort_years_is_a_no_op_without_an_order() {
        // C# `GetOrderBy` returns `Array.Empty` for an absent `sortBy`, and
        // `LibraryManager.Sort` then returns its input untouched.
        let mut years = vec![2020, 2021, 2022];
        sort_years(&mut years, &[]);
        assert_eq!(years, vec![2020, 2021, 2022]);
    }

    #[test]
    fn sort_years_honours_sort_name_in_both_directions() {
        let mut years = vec![2021, 2020, 2022];
        sort_years(&mut years, &[(ItemSortBy::SortName, SortOrder::Ascending)]);
        assert_eq!(years, vec![2020, 2021, 2022]);
        sort_years(&mut years, &[(ItemSortBy::SortName, SortOrder::Descending)]);
        assert_eq!(years, vec![2022, 2021, 2020]);
    }

    #[test]
    fn sort_years_leaves_unorderable_keys_alone() {
        // A `Year` carries no runtime to sort on; upstream's comparer would
        // compare equal, which is a stable no-op.
        let mut years = vec![2022, 2020, 2021];
        sort_years(&mut years, &[(ItemSortBy::Runtime, SortOrder::Ascending)]);
        assert_eq!(years, vec![2022, 2020, 2021]);
    }

    #[test]
    fn sort_years_random_keeps_the_same_multiset() {
        let mut years = vec![2020, 2021, 2022, 2023];
        sort_years(&mut years, &[(ItemSortBy::Random, SortOrder::Ascending)]);
        years.sort_unstable();
        assert_eq!(years, vec![2020, 2021, 2022, 2023]);
    }

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

    /// The `if (user is not null)` block in
    /// `MediaSourceManager.GetPlaybackMediaSources` (v10.11.8
    /// Emby.Server.Implementations/Library/MediaSourceManager.cs:204-217):
    /// audio takes only `EnableAudioPlaybackTranscoding`; video takes
    /// `EnableVideoPlaybackTranscoding` AND `EnablePlaybackRemuxing`; anything
    /// else is untouched. The overwrite is unconditional — it RAISES a flag the
    /// source left false, which is exactly why an HDHomeRun source (Protocol
    /// Udp, so the direct-stream validation cleared the flag) is still reported
    /// direct-streamable by Jellyfin.
    #[test]
    fn playback_permissions_overwrite_follows_the_media_type() {
        use crate::library::PlaybackPermissions;
        use ferrofin_model::dto::MediaSourceInfo;
        let perms = PlaybackPermissions {
            video_transcoding: false,
            audio_transcoding: true,
            remuxing: true,
        };
        let cleared = || MediaSourceInfo {
            supports_transcoding: false,
            supports_direct_stream: false,
            ..MediaSourceInfo::default()
        };

        let mut video = cleared();
        perms.apply(Some("Video"), &mut video);
        assert!(
            !video.supports_transcoding,
            "EnableVideoPlaybackTranscoding"
        );
        assert!(
            video.supports_direct_stream,
            "EnablePlaybackRemuxing RAISES a flag the protocol gate cleared"
        );

        let mut audio = cleared();
        perms.apply(Some("Audio"), &mut audio);
        assert!(audio.supports_transcoding, "EnableAudioPlaybackTranscoding");
        assert!(
            !audio.supports_direct_stream,
            "the audio arm never touches SupportsDirectStream"
        );

        for media_type in [None, Some("Photo"), Some("Book"), Some("Unknown")] {
            let mut other = cleared();
            perms.apply(media_type, &mut other);
            assert!(!other.supports_transcoding && !other.supports_direct_stream);
        }
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
