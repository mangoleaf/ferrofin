//! Metadata/image provider manager trait — refresh orchestration.
//!
//! Port of `MediaBrowser.Controller.Providers.IProviderManager` (the manager
//! only; the per-strategy `I*MetadataProvider` / `I*ImageProvider` / `IExternalId`
//! interfaces are intentionally **not** ported — they become match-on-item-kind
//! logic in `ferrofin-core`).
//!
//! Port rules applied:
//! - The C# `BaseItem` receivers become [`uuid::Uuid`] identity arguments; the
//!   image bytes overloads of `SaveImage` collapse to one `save_image` taking a
//!   MIME type plus owned bytes (a `Stream` cannot cross an object-safe async
//!   boundary cheaply).
//! - `AddParts` (registers the strategy interfaces that are not ported) and the
//!   `.NET` refresh events are dropped.
//! - Remote-image lookups reuse [`RemoteImageInfo`]/[`RemoteImageQuery`]/
//!   [`ImageProviderInfo`]; external-link lookups reuse
//!   [`ExternalIdInfo`]; metadata options reuse
//!   [`MetadataOptions`]/[`MetadataPluginSummary`] from `ferrofin-model`.
//! - The refresh-request/priority/update value types
//!   ([`MetadataRefreshOptions`], [`RefreshPriority`], [`ItemUpdateType`]) live
//!   under `MediaBrowser.Controller`/`.Model.Entities` and are ported here as
//!   local service-layer types. `ItemUpdateType` is a wire enum echoed in API
//!   responses and is flagged in the port report as a candidate for
//!   `ferrofin-model`.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` /
//!   `IProgress` are dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use std::collections::HashMap;

use async_trait::async_trait;
use ferrofin_model::configuration::{MetadataOptions, MetadataPluginSummary};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_model::providers::{
    ExternalIdInfo, ImageProviderInfo, ItemLookupInfo, RemoteImageInfo, RemoteImageQuery,
    RemoteSearchResult, SongInfo,
};
use uuid::Uuid;

use crate::error::ServiceError;

/// The priority a queued refresh runs at.
///
/// Port of `MediaBrowser.Controller.Providers.RefreshPriority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshPriority {
    /// A background/idle refresh.
    #[default]
    Low,
    /// A user-visible but non-blocking refresh.
    Normal,
    /// A refresh the caller is waiting on.
    High,
}

/// How aggressively a refresh should re-fetch and overwrite metadata.
///
/// Port of the `MetadataRefreshMode` enum carried by C#
/// `MetadataRefreshOptions`; only the mode field is retained (the directory
/// service / per-provider replace flags are impl-internal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataRefreshMode {
    /// Do not refresh.
    #[default]
    None,
    /// Validate only what is already present.
    ValidationOnly,
    /// Fetch missing metadata only.
    Default,
    /// Fetch all metadata (respecting cache).
    FullRefresh,
}

/// The options driving a metadata/image refresh.
///
/// Port of `MediaBrowser.Controller.Providers.MetadataRefreshOptions`, reduced
/// to the fields the manager surface actually needs. [`Default`] mirrors the
/// C# constructor: both modes `Default` (fetch what is missing), nothing
/// replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRefreshOptions {
    /// How metadata should be (re)fetched.
    pub metadata_refresh_mode: MetadataRefreshMode,
    /// How images should be (re)fetched.
    pub image_refresh_mode: MetadataRefreshMode,
    /// Replace all existing metadata rather than filling gaps.
    pub replace_all_metadata: bool,
    /// Replace all existing images rather than filling gaps.
    pub replace_all_images: bool,
    /// The "Identify" result the user chose (`POST /Items/RemoteSearch/Apply`):
    /// its provider ids replace the item's and bind the fetch to that exact
    /// record, its name/year drive any fallback search (the C#
    /// `MetadataService.ApplySearchResult`).
    pub search_result: Option<RemoteSearchResult>,
}

impl Default for MetadataRefreshOptions {
    fn default() -> Self {
        Self {
            metadata_refresh_mode: MetadataRefreshMode::Default,
            // `ImageRefreshOptions()` also defaults to `Default`.
            image_refresh_mode: MetadataRefreshMode::Default,
            replace_all_metadata: false,
            replace_all_images: false,
            search_result: None,
        }
    }
}

/// Which parts of an item a refresh changed.
///
/// Port of `MediaBrowser.Model.Entities.ItemUpdateType` (a `[Flags]` enum). It
/// is echoed in API responses, so it is a candidate for `ferrofin-model` — see
/// the port report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemUpdateType {
    /// Nothing changed.
    #[default]
    None,
    /// Only metadata was downloaded/updated.
    MetadataDownload,
    /// Metadata was edited (locally or imported).
    MetadataEdit,
    /// Images changed.
    ImageUpdate,
}

/// A type-erased remote metadata search request.
///
/// The C# `GetRemoteSearchResults<TItemType, TLookupType>` is generic over the
/// item type and the lookup-info type. Those generics cannot cross an
/// object-safe async boundary, so the handler collapses each concrete
/// `RemoteSearchQuery<XInfo>` into this single value: the item kind that selects
/// which remote providers apply, the shared [`ItemLookupInfo`] search criteria,
/// and the query knobs (`ItemId` reference, provider-name filter, and the
/// disabled-provider inclusion flag).
///
/// The type-specific extension fields of the concrete `*Info` types (album
/// artists, artist provider ids, contained song infos, music-video artists,
/// book series name) ride alongside the base so the per-kind fetchers
/// (MusicBrainz album/artist, …) see exactly what their C# `GetSearchResults`
/// overloads read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteSearchRequest {
    /// The kind of item being searched for (selects the applicable providers).
    pub item_kind: BaseItemKind,
    /// The shared lookup-info search criteria.
    pub search_info: ItemLookupInfo,
    /// The id of an existing item used as the search reference (nil when unset).
    pub item_id: Uuid,
    /// Restrict the search to the named provider when set.
    pub search_provider_name: Option<String>,
    /// Whether disabled providers should be included.
    pub include_disabled_providers: bool,
    /// `AlbumInfo.AlbumArtists` — the album's artists by name.
    pub album_artists: Vec<String>,
    /// `AlbumInfo.ArtistProviderIds` — the album artist's provider ids
    /// (`MusicBrainzArtist` drives the `arid:` release search).
    pub artist_provider_ids: Option<HashMap<String, String>>,
    /// `AlbumInfo.SongInfos` / `ArtistInfo.SongInfos` — the contained tracks,
    /// whose album artists / provider ids are the C# fallbacks.
    pub song_infos: Vec<SongInfo>,
    /// `MusicVideoInfo.Artists` — the music video's artists by name.
    pub artists: Vec<String>,
    /// `BookInfo.SeriesName` — the book's series.
    pub series_name: Option<String>,
}

/// Orchestrates metadata and image refreshing for library items.
///
/// Port of `IProviderManager` (manager surface only).
#[async_trait]
pub trait ProviderManager: Send + Sync {
    /// Queues an asynchronous refresh of an item at the given priority.
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
        priority: RefreshPriority,
    ) -> Result<(), ServiceError>;

    /// Refreshes an item and all of its children.
    async fn refresh_full_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError>;

    /// Refreshes a single item, returning what changed.
    async fn refresh_single_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError>;

    /// Downloads and stores an image for an item from a URL.
    async fn save_image_from_url(
        &self,
        item_id: Uuid,
        url: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError>;

    /// Stores caller-supplied image bytes for an item.
    async fn save_image(
        &self,
        item_id: Uuid,
        content: &[u8],
        mime_type: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError>;

    /// Deletes an item's image of `image_type` at `image_index` (default `0`).
    ///
    /// Port of `BaseItem.DeleteImageAsync(imageType, index)` (the fan-in target of
    /// `ImageController.DeleteItemImage`/`DeleteItemImageByIndex`): removes the
    /// on-disk file and the stored image row. The default implementation (for
    /// stub/test managers with no image store) reports the operation as unwired;
    /// the concrete provider manager overrides it.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] from a manager without an image store, or
    /// whatever error the concrete deletion surfaces.
    async fn delete_image(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let _ = (item_id, image_type, image_index);
        Err(ServiceError::backend(
            "delete_image needs an item-image store, which this provider manager has none of",
        ))
    }

    /// Gets the remote images available for an item.
    async fn get_available_remote_images(
        &self,
        item_id: Uuid,
        query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError>;

    /// Lists the remote image providers usable for an item.
    async fn get_remote_image_provider_info(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError>;

    /// Persists an item's metadata, recording the update type.
    async fn save_metadata(
        &self,
        item_id: Uuid,
        update_type: ItemUpdateType,
    ) -> Result<(), ServiceError>;

    /// Gets the external-id descriptors applicable to an item.
    async fn get_external_id_infos(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError>;

    /// Runs a remote metadata search and returns the deduplicated candidates.
    ///
    /// Port of `IProviderManager.GetRemoteSearchResults<TItemType, TLookupType>`:
    /// gathers the `IRemoteSearchProvider`s applicable to `request.item_kind`
    /// (optionally filtered to `request.search_provider_name`), queries each with
    /// the shared lookup info, stamps every result's `SearchProviderName`, and
    /// merges duplicates by shared provider id (first hit wins; later hits only
    /// fill in missing provider ids / image url).
    ///
    /// The default implementation (stub/test managers with no fetchers
    /// registered) returns an empty `Vec`, exactly as Jellyfin returns `[]`
    /// when no provider matches; the concrete provider manager fans out over
    /// its registered TMDB/TVDB/OMDb/MusicBrainz/TheAudioDb fetchers.
    ///
    /// # Errors
    ///
    /// [`ServiceError`] if resolving the reference item or a provider query fails
    /// (individual provider failures are swallowed by the port, matching the C#
    /// which logs and continues).
    async fn remote_search(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let _ = request;
        Ok(Vec::new())
    }

    /// Gets a summary of every registered metadata plugin.
    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError>;

    /// Assembles the available library options (metadata/image/subtitle/segment
    /// providers) for a library whose representative item types are `item_types`.
    ///
    /// Backs `GET /Libraries/AvailableOptions`. Defaults to an empty result so
    /// stub/test managers compile unchanged; the concrete provider manager
    /// overrides it to project the compiled-in provider registry.
    ///
    /// `is_new_library` is the request's `isNewLibrary` flag: the add-library
    /// wizard passes `true`, which changes which providers report
    /// `DefaultEnabled` (C# `IsSaverEnabledByDefault` /
    /// `IsMetadataFetcherEnabledByDefault` / `IsImageFetcherEnabledByDefault`),
    /// so a freshly created library does not silently start writing NFO
    /// sidecars or querying every remote provider.
    async fn get_library_options_info(
        &self,
        item_types: &[String],
        is_new_library: bool,
    ) -> Result<ferrofin_model::configuration::LibraryOptionsResultDto, ServiceError> {
        let _ = (item_types, is_new_library);
        Ok(ferrofin_model::configuration::LibraryOptionsResultDto::default())
    }

    /// Gets the configured metadata options for an item.
    async fn get_metadata_options(&self, item_id: Uuid) -> Result<MetadataOptions, ServiceError>;

    /// Gets the ids of items currently queued for refresh.
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError>;
}

fn _assert_object_safe_provider_manager(_: &dyn ProviderManager) {}

/// What a [`DynamicMetadataProvider`] is asked about: the item as scanned so
/// far. Plain data (no entity/DTO) so the seam stays stable for out-of-tree
/// implementations (the Tier-1b WASM plugin host).
#[derive(Debug, Clone, Default)]
pub struct DynamicMetadataLookup {
    /// The item's id.
    pub item_id: Uuid,
    /// The simple kind name (`Movie`, `Series`, `Episode`, …).
    pub kind: String,
    /// The item's display name.
    pub name: String,
    /// Release year, when known.
    pub production_year: Option<i32>,
    /// Filesystem path, when the item has one.
    pub path: Option<String>,
    /// External ids known so far, as (provider name, id) pairs.
    pub provider_ids: Vec<(String, String)>,
}

/// Metadata a dynamic provider contributes. **Supplement-only**: the scanner
/// applies each field only where the item still lacks a value — dynamic
/// providers fill gaps, they never overwrite built-in providers or user
/// edits.
#[derive(Debug, Clone, Default)]
pub struct DynamicMetadataResult {
    /// Plot/description text.
    pub overview: Option<String>,
    /// Release year.
    pub production_year: Option<i32>,
    /// Community rating on the 0–10 scale.
    pub community_rating: Option<f64>,
    /// Genre names (applied only when the item has none).
    pub genres: Vec<String>,
    /// External ids to record, as (provider name, id) pairs.
    pub provider_ids: Vec<(String, String)>,
    /// Tagline (supplement-only).
    pub tagline: Option<String>,
    /// Studio names (supplement-only).
    pub studios: Vec<String>,
    /// Tag names (supplement-only).
    pub tags: Vec<String>,
    /// Parental rating (supplement-only).
    pub official_rating: Option<String>,
    /// End date, ISO-8601 (supplement-only).
    pub end_date: Option<String>,
}

/// A dynamically-registered scan metadata source — the seam Tier-1b WASM
/// plugins implement (`metadata-lookup` in the `ferrofin:plugin` world).
/// Called per item AFTER the built-in provider chain (TVDB/TMDB/OMDb/NFO),
/// so built-ins stay authoritative and dynamic sources supplement.
///
/// Implementations must be cheap to call with `None`-meaning results: most
/// items are none of a given provider's business.
#[async_trait]
pub trait DynamicMetadataProvider: Send + Sync {
    /// A stable display name for logs (typically the plugin name).
    fn name(&self) -> &str;

    /// Whether this source is a NAMED provider the admin manages per
    /// library (its [`name`](Self::name) appears in the library-options
    /// fetcher lists). Gated sources are skipped/ordered by the library's
    /// `TypeOptions`; ungated sources always run, as before. Default:
    /// ungated.
    fn library_gated(&self) -> bool {
        false
    }

    /// Offers metadata for one item, or `Ok(None)` when this source has
    /// nothing to contribute.
    ///
    /// # Errors
    /// Provider-internal failure; the scanner logs it once and continues
    /// (one bad source never fails a scan).
    async fn lookup(
        &self,
        item: &DynamicMetadataLookup,
    ) -> Result<Option<DynamicMetadataResult>, ServiceError>;

    /// Downloads remote artwork for `item`, one image per wanted slot (the
    /// implementation fetches its own candidates and returns BYTES — for
    /// WASM plugins the host performs the download through the plugin's
    /// declared egress). Default: no artwork.
    async fn images(
        &self,
        item: &DynamicMetadataLookup,
        wanted: &[ferrofin_model::entities::ImageType],
    ) -> Result<Vec<(ferrofin_model::entities::ImageType, Vec<u8>)>, ServiceError> {
        let _ = (item, wanted);
        Ok(Vec::new())
    }
}

fn _assert_object_safe_dynamic_metadata_provider(_: &dyn DynamicMetadataProvider) {}

#[cfg(test)]
mod tests {
    use super::{ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, RefreshPriority};

    #[test]
    fn enums_default_to_the_no_op_variant() {
        assert_eq!(RefreshPriority::default(), RefreshPriority::Low);
        assert_eq!(MetadataRefreshMode::default(), MetadataRefreshMode::None);
        assert_eq!(ItemUpdateType::default(), ItemUpdateType::None);
    }

    #[test]
    fn refresh_options_default_mirrors_the_c_sharp_constructor() {
        let o = MetadataRefreshOptions::default();
        assert!(!o.replace_all_metadata);
        assert!(!o.replace_all_images);
        // `new MetadataRefreshOptions(directoryService)` sets the metadata
        // mode to `Default`, and its base `ImageRefreshOptions()` constructor
        // sets `ImageRefreshMode = Default` too (`ImageRefreshOptions.cs:14`).
        assert_eq!(o.metadata_refresh_mode, MetadataRefreshMode::Default);
        assert_eq!(o.image_refresh_mode, MetadataRefreshMode::Default);
        assert!(o.search_result.is_none());
    }
}
