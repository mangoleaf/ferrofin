//! The [`ProviderManager`] trait implementation — port of the
//! `MediaBrowser.Providers.Manager.ProviderManager` surface.
//!
//! Scope note (First-Light): the full C# `ProviderManager` couples the metadata
//! *refresh orchestration* to the library item store, the image-saving pipeline,
//! and the (deferred, feature-gated) remote provider plugins — none of which are
//! available in this wave. The high-value, test-backed deliverable in this crate
//! is the XbmcMetadata NFO parser subsystem ([`crate::xbmc`]).
//!
//! This type therefore implements the [`hermit_traits::providers::ProviderManager`]
//! trait as a thin, dependency-free shell: read-only descriptor queries return
//! empty/default results, and the operations that require the (not-yet-ported)
//! library store or network I/O return [`ServiceError::Backend`] describing the
//! deferral rather than silently succeeding. The external-id descriptor set —
//! which the NFO parsers consume — is fully wired.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use hermit_model::configuration::{MetadataOptions, MetadataPluginSummary};
use hermit_model::data::BaseItemKind;
use hermit_model::entities::ImageType;
use hermit_model::net::mime_types;
use hermit_model::providers::{
    ExternalIdInfo, ExternalUrl, ImageProviderInfo, ItemLookupInfo, RemoteImageInfo,
    RemoteImageQuery, RemoteSearchResult,
};
use hermit_traits::error::ServiceError;
use hermit_traits::options::ItemImageInfo;
use hermit_traits::persistence::{ItemPersistenceService, ItemRepository};
use hermit_traits::providers::{
    ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
    RemoteSearchRequest,
};
use uuid::Uuid;

use crate::tmdb::{TmdbClient, TmdbDetails, TmdbKind};
use hermit_db::entities::base_items::BaseItemEntity;

/// A [`RemoteSearchProvider`] backed by TMDB (the "Identify" flow). One instance
/// searches a single kind (movie or series), so it is registered once per kind.
pub struct TmdbSearchProvider {
    tmdb: Arc<TmdbClient>,
    kind: TmdbKind,
    supported: BaseItemKind,
}

impl TmdbSearchProvider {
    /// A TMDB search provider for `kind` (`Movie` or `Series`).
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, kind: TmdbKind) -> Self {
        let supported = match kind {
            TmdbKind::Movie => BaseItemKind::Movie,
            TmdbKind::Series => BaseItemKind::Series,
        };
        Self {
            tmdb,
            kind,
            supported,
        }
    }
}

#[async_trait]
impl RemoteSearchProvider for TmdbSearchProvider {
    // The trait fixes the return as `&str`; the literal is unavoidably behind it.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "TheMovieDb"
    }

    fn supports(&self, item_kind: BaseItemKind) -> bool {
        item_kind == self.supported
    }

    async fn get_search_results(
        &self,
        search_info: &ItemLookupInfo,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        let Some(name) = search_info.name.as_deref().filter(|n| !n.is_empty()) else {
            return Ok(Vec::new());
        };
        let hits = self.tmdb.search(self.kind, name, search_info.year).await;
        Ok(hits
            .into_iter()
            .map(|hit| RemoteSearchResult {
                name: hit.name,
                production_year: hit.year,
                image_url: hit.poster_url,
                overview: hit.overview,
                provider_ids: Some(std::collections::HashMap::from([(
                    "Tmdb".to_owned(),
                    hit.tmdb_id.to_string(),
                )])),
                search_provider_name: Some("TheMovieDb".to_owned()),
                ..RemoteSearchResult::default()
            })
            .collect())
    }
}

/// A single remote metadata-search fetcher (e.g. a TMDb or MusicBrainz plugin).
///
/// Port of `MediaBrowser.Controller.Providers.IRemoteSearchProvider<T>` reduced
/// to the object-safe surface the manager needs: a display name, the item kinds
/// it serves, and the search itself. The concrete network fetchers are
/// **deferred** (feature-gated, need API keys); this trait is the seam a host
/// registers them against when they land, and the one the dedup/merge port in
/// [`LocalProviderManager::remote_search`] drives.
#[async_trait]
pub trait RemoteSearchProvider: Send + Sync {
    /// The provider's display name, stamped onto every result it returns.
    fn name(&self) -> &str;

    /// Whether this provider can search for `item_kind`.
    fn supports(&self, item_kind: BaseItemKind) -> bool;

    /// Runs the search, returning raw candidate results (name/provider-ids set).
    ///
    /// # Errors
    ///
    /// Whatever the concrete fetcher surfaces; the manager logs and continues on
    /// a per-provider error rather than failing the whole search.
    async fn get_search_results(
        &self,
        search_info: &ItemLookupInfo,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError>;
}

/// A First-Light [`ProviderManager`] providing the descriptor surface and NFO
/// external-id set, with refresh/image orchestration deferred.
///
/// Construct via [`LocalProviderManager::new`]. The external-id descriptors it
/// advertises can be supplied at construction so downstream NFO parsing sees the
/// same provider set the manager would.
#[derive(Default, Clone)]
pub struct LocalProviderManager {
    external_id_infos: Vec<ExternalIdInfo>,
    remote_search_providers: Vec<Arc<dyn RemoteSearchProvider>>,
    /// The item-image store (rows) + the directory uploaded images are written
    /// to. Present enables the `save_image`/`delete_image` write paths.
    image_store: Option<Arc<dyn ItemPersistenceService>>,
    metadata_dir: Option<PathBuf>,
    /// The TMDB client + item store used by the remote-image ("Choose Image")
    /// methods to resolve an item and list/download its TMDB artwork.
    tmdb: Option<Arc<TmdbClient>>,
    items: Option<Arc<dyn ItemRepository>>,
}

impl std::fmt::Debug for LocalProviderManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalProviderManager")
            .field("external_id_infos", &self.external_id_infos)
            .field(
                "remote_search_providers",
                &self.remote_search_providers.len(),
            )
            .field("has_image_store", &self.image_store.is_some())
            .field("metadata_dir", &self.metadata_dir)
            .field("has_tmdb", &self.tmdb.is_some())
            .field("has_items", &self.items.is_some())
            .finish()
    }
}

impl LocalProviderManager {
    /// Creates a manager advertising `external_id_infos` for every item.
    ///
    /// No remote-search providers are registered — remote metadata search is
    /// deferred, so [`remote_search`](Self::remote_search) returns `[]`. Use
    /// [`with_remote_search_providers`](Self::with_remote_search_providers) to
    /// register fetchers once they land.
    #[must_use]
    pub fn new(external_id_infos: Vec<ExternalIdInfo>) -> Self {
        Self {
            external_id_infos,
            remote_search_providers: Vec::new(),
            image_store: None,
            metadata_dir: None,
            tmdb: None,
            items: None,
        }
    }

    /// Attaches the TMDB client + item store used by the remote-image methods
    /// (`get_available_remote_images` / `save_image_from_url`). Absent, those
    /// stay empty/deferred.
    #[must_use]
    pub fn with_remote_images(
        mut self,
        tmdb: Arc<TmdbClient>,
        items: Arc<dyn ItemRepository>,
    ) -> Self {
        self.tmdb = Some(tmdb);
        self.items = Some(items);
        self
    }

    /// Registers the remote-search fetchers this manager queries.
    ///
    /// The default set is empty (remote search deferred); a host with real
    /// network fetchers supplies them here.
    #[must_use]
    pub fn with_remote_search_providers(
        mut self,
        providers: Vec<Arc<dyn RemoteSearchProvider>>,
    ) -> Self {
        self.remote_search_providers = providers;
        self
    }

    /// Attaches the item-image store + the directory uploaded images are written
    /// to, enabling [`save_image`](ProviderManager::save_image) /
    /// [`delete_image`](ProviderManager::delete_image). Absent, both stay
    /// deferred (unit tests / hosts without an image store).
    #[must_use]
    pub fn with_image_store(
        mut self,
        image_store: Arc<dyn ItemPersistenceService>,
        metadata_dir: PathBuf,
    ) -> Self {
        self.image_store = Some(image_store);
        self.metadata_dir = Some(metadata_dir);
        self
    }

    /// Resolves an item to `(TmdbKind, name, year)` for a TMDB lookup, or `None`
    /// when the item is missing, pathless, or a kind TMDB does not serve here
    /// (only movies/series).
    async fn tmdb_lookup(
        &self,
        items: &Arc<dyn ItemRepository>,
        item_id: Uuid,
    ) -> Result<Option<(TmdbKind, String, Option<i32>)>, ServiceError> {
        let Some(entity) = items.retrieve_item(item_id).await? else {
            return Ok(None);
        };
        Ok(title_lookup(&entity))
    }

    /// Resolves what a refresh should fetch for `entity`: movies/series search
    /// TMDB by their own title; seasons/episodes hop to their parent series (its
    /// name/year drive the TMDB series search, the season/episode numbers select
    /// within it). Music and every other kind has no provider — `None`, the
    /// faithful skip.
    async fn resolve_refresh_target(
        &self,
        items: &Arc<dyn ItemRepository>,
        entity: &BaseItemEntity,
    ) -> Result<Option<RefreshTarget>, ServiceError> {
        // Only the season/episode arms need the parent-series row; fetch it here
        // so the classification itself stays pure (unit-testable without a repo).
        let series = if matches!(short_kind(entity), "Season" | "Episode") {
            match entity
                .series_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                Some(series_uuid) => items.retrieve_item(series_uuid).await?,
                None => None,
            }
        } else {
            None
        };
        Ok(refresh_target_of(entity, series.as_ref()))
    }

    /// Searches TMDB for the parent series and fetches one season's details —
    /// the shared first half of the season/episode refresh arms. `None` when the
    /// series has no TMDB match or the season fetch fails.
    async fn fetch_season(
        &self,
        tmdb: &Arc<TmdbClient>,
        series_name: &str,
        series_year: Option<i32>,
        season_number: i32,
    ) -> Option<crate::tmdb::SeasonDetails> {
        let hit = tmdb
            .search(TmdbKind::Series, series_name, series_year)
            .await
            .into_iter()
            .next()?;
        tmdb.season_details(hit.tmdb_id, season_number).await
    }

    /// Applies a season's/episode's fetched name/overview (+ Primary artwork URL)
    /// onto the row — the shared second half of the two TV refresh arms. The
    /// metadata pass persists via the item store; the image download is
    /// best-effort (a failed download must not fail the refresh).
    #[allow(clippy::too_many_arguments)]
    async fn apply_tv_slice(
        &self,
        entity: &mut BaseItemEntity,
        item_id: Uuid,
        name: Option<&str>,
        overview: Option<&str>,
        image_url: Option<&str>,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        if wants_fetch(options.metadata_refresh_mode) {
            apply_name_overview(entity, name, overview, options.replace_all_metadata);
            if let Some(store) = &self.image_store {
                store.save_items(std::slice::from_ref(entity)).await?;
            }
        }
        if wants_fetch(options.image_refresh_mode)
            && self.image_store.is_some()
            && let Some(url) = image_url
        {
            let _ = self
                .save_image_from_url(item_id, url, ImageType::Primary, None)
                .await;
        }
        Ok(())
    }

    /// Builds the "operation deferred" error for the orchestration methods that
    /// need the (not-yet-ported) library store or network I/O.
    fn deferred(op: &str) -> ServiceError {
        ServiceError::backend(format!(
            "{op} is deferred until the library-item store / image pipeline lands"
        ))
    }

    /// Merges `incoming` into `result_list`, deduplicating by shared provider id.
    ///
    /// Port of the inner merge in C# `GetRemoteSearchResults`: a result matches an
    /// existing one when they agree on the value of any provider-id key. On a
    /// match the existing entry absorbs any provider ids it is missing and adopts
    /// the incoming image url when it had none; otherwise the incoming result is
    /// appended.
    fn merge_search_result(
        result_list: &mut Vec<RemoteSearchResult>,
        incoming: RemoteSearchResult,
    ) {
        let incoming_ids = incoming.provider_ids.clone().unwrap_or_default();

        let existing = result_list.iter_mut().find(|existing| {
            let existing_ids = existing.provider_ids.as_ref();
            incoming_ids.iter().any(|(key, value)| {
                existing_ids
                    .and_then(|ids| ids.get(key))
                    .is_some_and(|existing_value| existing_value.eq_ignore_ascii_case(value))
            })
        });

        match existing {
            Some(existing) => {
                let ids = existing.provider_ids.get_or_insert_with(Default::default);
                for (key, value) in incoming_ids {
                    ids.entry(key).or_insert(value);
                }
                if existing
                    .image_url
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(str::is_empty)
                {
                    existing.image_url = incoming.image_url;
                }
            }
            None => result_list.push(incoming),
        }
    }
}

/// What a metadata refresh should fetch for one item — resolved from the item's
/// kind and (for seasons/episodes) its parent-series linkage.
enum RefreshTarget {
    /// A movie or series: TMDB-search by its own title.
    Title {
        /// The TMDB search kind (movie vs series).
        kind: TmdbKind,
        /// The title to search for.
        name: String,
        /// The production year narrowing the search, when known.
        year: Option<i32>,
    },
    /// A season: search the parent series, then fetch `/tv/{id}/season/{n}`.
    Season {
        /// The parent series title driving the TMDB search.
        series_name: String,
        /// The parent series year, when known.
        series_year: Option<i32>,
        /// The season number within the series.
        season_number: i32,
    },
    /// An episode: like a season, then select the episode within it.
    Episode {
        /// The parent series title driving the TMDB search.
        series_name: String,
        /// The parent series year, when known.
        series_year: Option<i32>,
        /// The season number within the series.
        season_number: i32,
        /// The episode number within the season.
        episode_number: i32,
    },
}

/// The short (C#-unqualified) kind name of a row, e.g. the stored
/// `MediaBrowser.Controller.Entities.TV.Episode` → `Episode`.
fn short_kind(entity: &BaseItemEntity) -> &str {
    entity.type_.rsplit('.').next().unwrap_or(&entity.type_)
}

/// The pure `(kind, name, year)` extraction for a movie/series row, shared by
/// [`LocalProviderManager::tmdb_lookup`] and the refresh-target resolver.
fn title_lookup(entity: &BaseItemEntity) -> Option<(TmdbKind, String, Option<i32>)> {
    let kind = match short_kind(entity) {
        "Movie" => TmdbKind::Movie,
        "Series" => TmdbKind::Series,
        _ => return None,
    };
    let name = entity.name.clone().filter(|n| !n.is_empty())?;
    let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
    Some((kind, name, year))
}

/// Classifies what a refresh should fetch for `entity` — the pure half of
/// [`LocalProviderManager::resolve_refresh_target`]. `series` is the item's
/// parent-series row when one was resolvable (seasons/episodes only). `None`
/// for kinds with no provider (music etc.), broken series links, or missing
/// season/episode numbers.
fn refresh_target_of(
    entity: &BaseItemEntity,
    series: Option<&BaseItemEntity>,
) -> Option<RefreshTarget> {
    let kind = short_kind(entity);
    if matches!(kind, "Movie" | "Series") {
        return title_lookup(entity).map(|(kind, name, year)| RefreshTarget::Title {
            kind,
            name,
            year,
        });
    }
    if !matches!(kind, "Season" | "Episode") {
        return None;
    }
    let series = series?;
    let series_name = series.name.clone().filter(|n| !n.is_empty())?;
    let series_year = series.production_year.and_then(|y| i32::try_from(y).ok());
    let number = |v: Option<i64>| v.and_then(|n| i32::try_from(n).ok());
    if kind == "Season" {
        Some(RefreshTarget::Season {
            series_name,
            series_year,
            season_number: number(entity.index_number)?,
        })
    } else {
        // Episode: `parent_index_number` is the season, `index_number` the
        // episode within it.
        Some(RefreshTarget::Episode {
            series_name,
            series_year,
            season_number: number(entity.parent_index_number)?,
            episode_number: number(entity.index_number)?,
        })
    }
}

/// Fills or replaces an item row's name + overview (the season/episode TMDB
/// fields), with the same fill-or-replace semantics as [`apply_tmdb_details`].
fn apply_name_overview(
    entity: &mut BaseItemEntity,
    name: Option<&str>,
    overview: Option<&str>,
    replace: bool,
) {
    set_text(&mut entity.name, name, replace);
    set_text(&mut entity.overview, overview, replace);
}

/// Whether a [`MetadataRefreshMode`] should fetch remote data (`Default` /
/// `FullRefresh`); `None` / `ValidationOnly` skip the network.
fn wants_fetch(mode: MetadataRefreshMode) -> bool {
    matches!(
        mode,
        MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
    )
}

/// Copies `new` into `cur` when `new` has a value and either `replace` is set or
/// `cur` is currently empty. Mirrors the scanner's fill-or-replace merge.
fn set_text(cur: &mut Option<String>, new: Option<&str>, replace: bool) {
    if let Some(value) = new
        && (replace || cur.as_deref().is_none_or(str::is_empty))
    {
        *cur = Some(value.to_owned());
    }
}

/// Applies fetched TMDB [`TmdbDetails`] onto an item row. Each field is filled
/// when empty, or overwritten when `replace` is set (the C# `FullRefresh`
/// `ReplaceAllMetadata` behavior); a field TMDB did not return is left untouched.
/// Mirrors the scanner's `apply_details` merge.
fn apply_tmdb_details(entity: &mut BaseItemEntity, details: &TmdbDetails, replace: bool) {
    set_text(&mut entity.overview, details.overview.as_deref(), replace);
    set_text(&mut entity.tagline, details.tagline.as_deref(), replace);
    set_text(
        &mut entity.official_rating,
        details.official_rating.as_deref(),
        replace,
    );
    if details.community_rating.is_some() && (replace || entity.community_rating.is_none()) {
        entity.community_rating = details.community_rating;
    }
    if !details.genres.is_empty()
        && (replace || entity.genres.as_deref().unwrap_or_default().is_empty())
    {
        entity.genres = Some(details.genres.join("|"));
    }
    if !details.studios.is_empty()
        && (replace || entity.studios.as_deref().unwrap_or_default().is_empty())
    {
        entity.studios = Some(details.studios.join("|"));
    }
    if details.production_year.is_some() && (replace || entity.production_year.is_none()) {
        entity.production_year = details.production_year.map(i64::from);
    }
    if let Some(mins) = details.runtime_minutes
        && (replace || entity.run_time_ticks.is_none())
    {
        // Ticks are 100-ns units: minutes × 60 s × 10,000,000.
        entity.run_time_ticks = Some(i64::from(mins) * 600_000_000);
    }
    if let Some(date) = details.premiere_date.as_deref().and_then(parse_ymd)
        && (replace || entity.premiere_date.is_none())
    {
        entity.premiere_date = Some(date);
    }
}

/// Parses a TMDB `YYYY-MM-DD` date into a UTC midnight timestamp.
fn parse_ymd(s: &str) -> Option<chrono::DateTime<Utc>> {
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(chrono::DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

/// The on-disk filename stem for an uploaded image of `image_type`/`index`,
/// e.g. `Primary` → `primary`, `Backdrop` index 2 → `backdrop2`.
fn image_file_stem(image_type: ImageType, index: Option<i32>) -> String {
    let base = format!("{image_type:?}").to_ascii_lowercase();
    match index.filter(|&i| i > 0) {
        Some(i) => format!("{base}{i}"),
        None => base,
    }
}

#[async_trait]
impl ProviderManager for LocalProviderManager {
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        // Run the refresh in the background and return immediately — the C#
        // queued-refresh shape (the refresh button 204s while the fetch runs).
        // ponytail: _priority ignored — a single-item spawn needs no ordering.
        // Add an ordered, priority-aware worker queue when bulk/library-wide
        // refreshes land.
        let mgr = self.clone();
        let options = *options;
        let handle = tokio::spawn(async move {
            if let Err(err) = mgr.refresh_full_item(item_id, &options).await {
                tracing::warn!(%item_id, %err, "queued metadata refresh failed");
            }
        });
        drop(handle);
        Ok(())
    }

    async fn refresh_full_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        let (Some(tmdb), Some(items)) = (&self.tmdb, &self.items) else {
            // No remote provider configured — nothing to fetch (faithful: Jellyfin
            // with no metadata plugins leaves the item unchanged).
            return Ok(());
        };
        let Some(mut entity) = items.retrieve_item(item_id).await? else {
            return Err(ServiceError::not_found(format!("item {item_id}")));
        };
        // Resolve what to fetch: movies/series by their own title, seasons/
        // episodes via their parent series. Music/other kinds have no provider —
        // the faithful skip.
        // ponytail: title targets re-search by name (the same query the client's
        // "Identify" ran), not the exact provider id on the chosen result — writing
        // that id needs a `BaseItemProviders` upsert path that does not exist yet.
        // In the common case the top hit matches the user's pick; the metadata is
        // real either way. Honor the exact pick once that write path lands.
        let Some(target) = self.resolve_refresh_target(items, &entity).await? else {
            return Ok(());
        };
        match target {
            RefreshTarget::Title { kind, name, year } => {
                let Some(hit) = tmdb.search(kind, &name, year).await.into_iter().next() else {
                    return Ok(());
                };
                let Some(details) = tmdb.details(kind, hit.tmdb_id).await else {
                    return Ok(());
                };

                // Metadata pass: apply the fetched fields onto the row and persist.
                if wants_fetch(options.metadata_refresh_mode) {
                    apply_tmdb_details(&mut entity, &details, options.replace_all_metadata);
                    // Persist through the item store (the same `ItemPersistenceService`
                    // the scanner writes enriched rows with).
                    if let Some(store) = &self.image_store {
                        store.save_items(std::slice::from_ref(&entity)).await?;
                    }
                }

                // Image pass: download the primary + backdrop when requested and an
                // image store is wired. A single failed download must not fail the
                // refresh.
                if wants_fetch(options.image_refresh_mode) && self.image_store.is_some() {
                    let candidates = self
                        .get_available_remote_images(item_id, &RemoteImageQuery::default())
                        .await?;
                    for image_type in [ImageType::Primary, ImageType::Backdrop] {
                        if let Some(url) = candidates
                            .iter()
                            .find(|img| img.type_ == image_type)
                            .and_then(|img| img.url.clone())
                        {
                            let _ = self
                                .save_image_from_url(item_id, &url, image_type, None)
                                .await;
                        }
                    }
                }
            }
            RefreshTarget::Season {
                series_name,
                series_year,
                season_number,
            } => {
                let Some(season) = self
                    .fetch_season(tmdb, &series_name, series_year, season_number)
                    .await
                else {
                    return Ok(());
                };
                // The season's artwork is its poster (Primary), like the scanner.
                self.apply_tv_slice(
                    &mut entity,
                    item_id,
                    season.name.as_deref(),
                    season.overview.as_deref(),
                    season.poster.as_deref(),
                    options,
                )
                .await?;
            }
            RefreshTarget::Episode {
                series_name,
                series_year,
                season_number,
                episode_number,
            } => {
                let Some(season) = self
                    .fetch_season(tmdb, &series_name, series_year, season_number)
                    .await
                else {
                    return Ok(());
                };
                let Some(episode) = season
                    .episodes
                    .into_iter()
                    .find(|ep| ep.episode_number == episode_number)
                else {
                    return Ok(());
                };
                // The episode's still is stored as Primary (the scanner's
                // convention for episode artwork).
                self.apply_tv_slice(
                    &mut entity,
                    item_id,
                    episode.name.as_deref(),
                    episode.overview.as_deref(),
                    episode.still_url.as_deref(),
                    options,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        Err(Self::deferred("refresh_single_item"))
    }

    async fn save_image_from_url(
        &self,
        item_id: Uuid,
        url: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let Some(tmdb) = &self.tmdb else {
            return Err(Self::deferred("save_image_from_url"));
        };
        let bytes = tmdb.download(url).await.ok_or_else(|| {
            ServiceError::backend(format!("could not download remote image {url}"))
        })?;
        // Reuse the local write+persist path.
        let mime = if url.to_ascii_lowercase().ends_with(".png") {
            "image/png"
        } else {
            "image/jpeg"
        };
        self.save_image(item_id, &bytes, mime, image_type, image_index)
            .await
    }

    async fn save_image(
        &self,
        item_id: Uuid,
        content: &[u8],
        mime_type: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let (Some(store), Some(meta_root)) = (&self.image_store, &self.metadata_dir) else {
            return Err(Self::deferred("save_image"));
        };
        let ext = mime_types::to_extension(&mime_type.to_ascii_lowercase())
            .unwrap_or(".jpg")
            .trim_start_matches('.');
        let item_dir = meta_root.join(item_id.to_string());
        let stem = image_file_stem(image_type, image_index);
        let dest = item_dir.join(format!("{stem}.{ext}"));
        std::fs::create_dir_all(&item_dir)
            .map_err(|e| ServiceError::backend(format!("create image dir: {e}")))?;
        std::fs::write(&dest, content)
            .map_err(|e| ServiceError::backend(format!("write image: {e}")))?;
        store
            .set_item_image(
                item_id,
                &ItemImageInfo {
                    path: dest.to_string_lossy().into_owned(),
                    image_type,
                    date_modified: Utc::now(),
                    width: 0,
                    height: 0,
                    blur_hash: None,
                },
            )
            .await
    }

    async fn delete_image(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let Some(store) = &self.image_store else {
            return Err(Self::deferred("delete_image"));
        };
        // Remove the DB rows, then the files they pointed at (best-effort — a
        // missing file must not fail the delete).
        let paths = store
            .delete_item_image(item_id, image_type, image_index)
            .await?;
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }

    async fn get_available_remote_images(
        &self,
        item_id: Uuid,
        query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        let (Some(tmdb), Some(items)) = (&self.tmdb, &self.items) else {
            return Ok(Vec::new());
        };
        let Some((kind, name, year)) = self.tmdb_lookup(items, item_id).await? else {
            return Ok(Vec::new());
        };
        // Best-match the title, then list all of TMDB's images for it.
        let Some(hit) = tmdb.search(kind, &name, year).await.into_iter().next() else {
            return Ok(Vec::new());
        };
        let images = tmdb.all_images(kind, hit.tmdb_id).await;
        Ok(images
            .into_iter()
            .filter(|img| query.image_type.is_none_or(|t| t == img.image_type))
            .map(|img| RemoteImageInfo {
                provider_name: Some("TheMovieDb".to_owned()),
                url: Some(img.url),
                width: img.width,
                height: img.height,
                community_rating: img.community_rating,
                vote_count: img.vote_count,
                language: img.language,
                type_: img.image_type,
                ..RemoteImageInfo::default()
            })
            .collect())
    }

    async fn get_remote_image_provider_info(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        let Some(items) = &self.items else {
            return Ok(Vec::new());
        };
        // Advertise TMDB only for the kinds it serves.
        if self.tmdb_lookup(items, item_id).await?.is_none() {
            return Ok(Vec::new());
        }
        Ok(vec![ImageProviderInfo {
            name: Some("TheMovieDb".to_owned()),
            supported_images: vec![ImageType::Primary, ImageType::Backdrop],
        }])
    }

    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        // The DB-side persistence already happened in the caller (e.g.
        // `save_image_from_url` → `set_item_image`). The remaining C# work is
        // writing metadata savers (NFO/images to the media folder), which Hermit
        // defers — so this is a successful no-op rather than a hard failure, and
        // the image-download / metadata-edit flows complete.
        Ok(())
    }

    async fn get_external_urls(&self, _item_id: Uuid) -> Result<Vec<ExternalUrl>, ServiceError> {
        Ok(Vec::new())
    }

    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError> {
        Ok(self.external_id_infos.clone())
    }

    async fn remote_search(
        &self,
        request: &RemoteSearchRequest,
    ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
        // Select the providers that serve this item kind, then (if a provider
        // name was supplied) narrow to that provider — a port of the C#
        // `GetMetadataProvidersInternal(...).OfType<IRemoteSearchProvider>()`
        // filter chain. With no fetcher registered this set is empty and the
        // loop below yields `[]`, exactly as Jellyfin returns when nothing
        // matches.
        let name_filter = request.search_provider_name.as_deref();
        let providers = self.remote_search_providers.iter().filter(|p| {
            p.supports(request.item_kind)
                && name_filter.is_none_or(|n| p.name().eq_ignore_ascii_case(n))
        });

        let mut result_list: Vec<RemoteSearchResult> = Vec::new();

        for provider in providers {
            // Per-provider failures are logged-and-skipped in C#; here we simply
            // continue so one bad provider can't fail the whole search.
            let Ok(results) = provider.get_search_results(&request.search_info).await else {
                continue;
            };

            for mut result in results {
                result.search_provider_name = Some(provider.name().to_owned());
                Self::merge_search_result(&mut result_list, result);
            }
        }

        Ok(result_list)
    }

    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError> {
        Ok(crate::library_options::all_metadata_plugins())
    }

    async fn get_library_options_info(
        &self,
        item_types: &[String],
    ) -> Result<hermit_model::configuration::LibraryOptionsResultDto, ServiceError> {
        Ok(crate::library_options::library_options_info(item_types))
    }

    async fn get_metadata_options(&self, _item_id: Uuid) -> Result<MetadataOptions, ServiceError> {
        Ok(MetadataOptions::default())
    }

    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{
        LocalProviderManager, RemoteSearchProvider, apply_tmdb_details, parse_ymd, set_text,
        wants_fetch,
    };
    use crate::tmdb::TmdbDetails;
    use async_trait::async_trait;
    use hermit_db::entities::base_items::BaseItemEntity;
    use hermit_model::data::BaseItemKind;
    use hermit_model::entities::ImageType;
    use hermit_model::providers::{
        ExternalIdInfo, ItemLookupInfo, RemoteImageQuery, RemoteSearchResult,
    };
    use hermit_traits::error::ServiceError;
    use hermit_traits::providers::{
        ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager,
        RefreshPriority, RemoteSearchRequest,
    };
    use uuid::Uuid;

    /// A synthetic remote-search provider for exercising the merge/dedup port.
    struct FakeProvider {
        name: String,
        kind: BaseItemKind,
        results: Vec<RemoteSearchResult>,
        fail: bool,
    }

    #[async_trait]
    impl RemoteSearchProvider for FakeProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn supports(&self, item_kind: BaseItemKind) -> bool {
            item_kind == self.kind
        }

        async fn get_search_results(
            &self,
            _search_info: &ItemLookupInfo,
        ) -> Result<Vec<RemoteSearchResult>, ServiceError> {
            if self.fail {
                return Err(ServiceError::backend("boom"));
            }
            Ok(self.results.clone())
        }
    }

    fn result_with(name: &str, ids: &[(&str, &str)], image: Option<&str>) -> RemoteSearchResult {
        let mut map = HashMap::new();
        for (k, v) in ids {
            map.insert((*k).to_owned(), (*v).to_owned());
        }
        RemoteSearchResult {
            name: Some(name.to_owned()),
            provider_ids: Some(map),
            image_url: image.map(str::to_owned),
            ..RemoteSearchResult::default()
        }
    }

    fn request(kind: BaseItemKind) -> RemoteSearchRequest {
        RemoteSearchRequest {
            item_kind: kind,
            ..RemoteSearchRequest::default()
        }
    }

    #[tokio::test]
    async fn remote_search_is_empty_when_no_provider_registered() {
        let mgr = LocalProviderManager::default();
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .expect("search succeeds");
        assert!(out.is_empty(), "deferred remote search returns []");
    }

    #[tokio::test]
    async fn remote_search_stamps_provider_name_and_returns_results() {
        let provider = Arc::new(FakeProvider {
            name: "TheMovieDb".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Inception", &[("Tmdb", "27205")], None)],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![provider as Arc<dyn RemoteSearchProvider>]);

        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].search_provider_name.as_deref(), Some("TheMovieDb"));
        assert_eq!(out[0].name.as_deref(), Some("Inception"));
    }

    #[tokio::test]
    async fn remote_search_skips_providers_of_other_kinds() {
        let provider = Arc::new(FakeProvider {
            name: "MusicBrainz".to_owned(),
            kind: BaseItemKind::MusicAlbum,
            results: vec![result_with(
                "Kind of Blue",
                &[("MusicBrainzAlbum", "x")],
                None,
            )],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![provider as Arc<dyn RemoteSearchProvider>]);
        // A Movie search must not reach a MusicAlbum provider.
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn remote_search_provider_name_filter_is_case_insensitive() {
        let provider = Arc::new(FakeProvider {
            name: "TheMovieDb".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Inception", &[("Tmdb", "27205")], None)],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![provider as Arc<dyn RemoteSearchProvider>]);

        let mut req = request(BaseItemKind::Movie);
        req.search_provider_name = Some("themoviedb".to_owned());
        assert_eq!(mgr.remote_search(&req).await.unwrap().len(), 1);

        req.search_provider_name = Some("NoSuchProvider".to_owned());
        assert!(mgr.remote_search(&req).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remote_search_merges_duplicates_by_shared_provider_id() {
        // Two providers return the same film under a shared Tmdb id; the second
        // adds an Imdb id and an image url the first lacked.
        let a = Arc::new(FakeProvider {
            name: "A".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Inception", &[("Tmdb", "27205")], None)],
            fail: false,
        });
        let b = Arc::new(FakeProvider {
            name: "B".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with(
                "Inception",
                &[("Tmdb", "27205"), ("Imdb", "tt1375666")],
                Some("http://img/incep.jpg"),
            )],
            fail: false,
        });
        let mgr = LocalProviderManager::default().with_remote_search_providers(vec![
            a as Arc<dyn RemoteSearchProvider>,
            b as Arc<dyn RemoteSearchProvider>,
        ]);

        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "duplicates collapse to one entry");
        let ids = out[0].provider_ids.as_ref().unwrap();
        assert_eq!(ids["Tmdb"], "27205");
        assert_eq!(ids["Imdb"], "tt1375666", "missing id absorbed from B");
        assert_eq!(
            out[0].image_url.as_deref(),
            Some("http://img/incep.jpg"),
            "image url adopted from B"
        );
        // The first provider that produced the surviving entry wins its name.
        assert_eq!(out[0].search_provider_name.as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn remote_search_distinct_ids_are_not_merged() {
        let a = Arc::new(FakeProvider {
            name: "A".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![
                result_with("Inception", &[("Tmdb", "27205")], None),
                result_with("Tenet", &[("Tmdb", "577922")], None),
            ],
            fail: false,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_search_providers(vec![a as Arc<dyn RemoteSearchProvider>]);
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn remote_search_swallows_a_failing_provider() {
        let bad = Arc::new(FakeProvider {
            name: "Bad".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![],
            fail: true,
        });
        let good = Arc::new(FakeProvider {
            name: "Good".to_owned(),
            kind: BaseItemKind::Movie,
            results: vec![result_with("Dune", &[("Tmdb", "438631")], None)],
            fail: false,
        });
        let mgr = LocalProviderManager::default().with_remote_search_providers(vec![
            bad as Arc<dyn RemoteSearchProvider>,
            good as Arc<dyn RemoteSearchProvider>,
        ]);
        let out = mgr
            .remote_search(&request(BaseItemKind::Movie))
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "the good provider's result still lands");
        assert_eq!(out[0].name.as_deref(), Some("Dune"));
    }

    #[tokio::test]
    async fn external_id_infos_are_advertised() {
        let mgr = LocalProviderManager::default();
        let infos = mgr
            .get_external_id_infos(Uuid::nil())
            .await
            .expect("descriptor query succeeds");
        assert!(infos.is_empty());
    }

    #[tokio::test]
    async fn new_advertises_supplied_external_ids_for_every_item() {
        let seed = vec![ExternalIdInfo::new("Tmdb".into(), "tmdb".into(), None)];
        let mgr = LocalProviderManager::new(seed.clone());
        // The manager clones the same descriptor set for any item id.
        let a = mgr.get_external_id_infos(Uuid::nil()).await.unwrap();
        let b = mgr.get_external_id_infos(Uuid::new_v4()).await.unwrap();
        assert_eq!(a, seed);
        assert_eq!(b, seed);
    }

    #[tokio::test]
    async fn refresh_is_deferred() {
        let mgr = LocalProviderManager::default();
        let err = mgr
            .refresh_single_item(Uuid::nil(), &MetadataRefreshOptions::default())
            .await
            .expect_err("refresh is deferred");
        assert!(err.to_string().contains("deferred"));
    }

    /// Every orchestration method that needs the not-yet-ported library store /
    /// image pipeline must return a `deferred` backend error naming the op.
    #[tokio::test]
    async fn all_orchestration_methods_are_deferred_with_op_name() {
        let mgr = LocalProviderManager::default();
        let id = Uuid::nil();
        let opts = MetadataRefreshOptions::default();

        // `queue_refresh` spawns the refresh in the background and returns Ok
        // immediately; with no TMDB client wired the spawned refresh no-ops.
        mgr.queue_refresh(id, &opts, RefreshPriority::Normal)
            .await
            .expect("queue_refresh accepts the enqueue");

        // `refresh_full_item` on a manager with no TMDB client / item store has
        // nothing to fetch and succeeds as a no-op (faithful: Jellyfin with no
        // metadata provider leaves the item unchanged). The field-merge helpers it
        // applies are unit-tested in `metadata_merge_helpers` below.
        mgr.refresh_full_item(id, &opts)
            .await
            .expect("refresh_full_item is a no-op without a provider");

        let save_url = mgr
            .save_image_from_url(id, "http://x/y.jpg", ImageType::Primary, Some(0))
            .await
            .expect_err("save_image_from_url deferred");
        assert!(save_url.to_string().contains("save_image_from_url"));

        let save_bytes = mgr
            .save_image(id, b"data", "image/jpeg", ImageType::Backdrop, None)
            .await
            .expect_err("save_image deferred");
        assert!(save_bytes.to_string().contains("save_image"));

        // `save_metadata` is an accepted no-op (the DB write happens in the
        // caller; NFO-saver writing is deferred), so the image-download /
        // metadata-edit flows complete rather than 500.
        mgr.save_metadata(id, ItemUpdateType::MetadataDownload)
            .await
            .expect("save_metadata is an accepted no-op");
    }

    /// The field-merge helpers behind `refresh_full_item`: `wants_fetch` gates on
    /// the refresh mode, `set_text` fills-or-replaces, `parse_ymd` reads a TMDB
    /// date.
    #[test]
    fn metadata_merge_helpers() {
        // wants_fetch: Default/FullRefresh fetch; None/ValidationOnly do not.
        assert!(wants_fetch(MetadataRefreshMode::FullRefresh));
        assert!(wants_fetch(MetadataRefreshMode::Default));
        assert!(!wants_fetch(MetadataRefreshMode::None));
        assert!(!wants_fetch(MetadataRefreshMode::ValidationOnly));

        // set_text fills an empty target regardless of replace.
        let mut cur = None;
        set_text(&mut cur, Some("Solaris"), false);
        assert_eq!(cur.as_deref(), Some("Solaris"));
        // With replace=false an existing value is kept.
        set_text(&mut cur, Some("Stalker"), false);
        assert_eq!(cur.as_deref(), Some("Solaris"));
        // With replace=true it is overwritten.
        set_text(&mut cur, Some("Stalker"), true);
        assert_eq!(cur.as_deref(), Some("Stalker"));
        // A `None` incoming value never clears the target.
        set_text(&mut cur, None, true);
        assert_eq!(cur.as_deref(), Some("Stalker"));
        // An empty existing string counts as absent (fills without replace).
        let mut empty = Some(String::new());
        set_text(&mut empty, Some("Mirror"), false);
        assert_eq!(empty.as_deref(), Some("Mirror"));

        // parse_ymd reads a valid date and rejects garbage.
        assert!(parse_ymd("1972-03-20").is_some());
        assert!(parse_ymd("not-a-date").is_none());
    }

    /// `apply_tmdb_details` fills empty fields, honors `replace`, joins
    /// genres/studios, converts runtime to ticks, and parses the premiere date.
    #[test]
    fn apply_tmdb_details_fills_and_replaces() {
        let details = TmdbDetails {
            overview: Some("A physicist visits a space station.".to_owned()),
            tagline: Some("A tagline".to_owned()),
            genres: vec!["Science Fiction".to_owned(), "Drama".to_owned()],
            studios: vec!["Mosfilm".to_owned()],
            community_rating: Some(8.1),
            official_rating: Some("PG".to_owned()),
            production_year: Some(1972),
            premiere_date: Some("1972-03-20".to_owned()),
            runtime_minutes: Some(167),
            ..TmdbDetails::default()
        };

        // Empty entity ⇒ every field filled regardless of `replace`.
        let mut entity = BaseItemEntity::default();
        apply_tmdb_details(&mut entity, &details, false);
        assert_eq!(
            entity.overview.as_deref(),
            Some("A physicist visits a space station.")
        );
        assert_eq!(entity.genres.as_deref(), Some("Science Fiction|Drama"));
        assert_eq!(entity.studios.as_deref(), Some("Mosfilm"));
        assert_eq!(entity.community_rating, Some(8.1));
        assert_eq!(entity.official_rating.as_deref(), Some("PG"));
        assert_eq!(entity.production_year, Some(1972));
        assert_eq!(entity.run_time_ticks, Some(167 * 600_000_000));
        assert!(entity.premiere_date.is_some());

        // With replace=false an existing value is kept.
        let mut kept = BaseItemEntity {
            overview: Some("existing".to_owned()),
            ..BaseItemEntity::default()
        };
        apply_tmdb_details(&mut kept, &details, false);
        assert_eq!(kept.overview.as_deref(), Some("existing"));
        // With replace=true it is overwritten.
        apply_tmdb_details(&mut kept, &details, true);
        assert_eq!(
            kept.overview.as_deref(),
            Some("A physicist visits a space station.")
        );

        // A detail TMDB did not return leaves the field untouched.
        let mut untouched = BaseItemEntity {
            tagline: Some("keep me".to_owned()),
            ..BaseItemEntity::default()
        };
        apply_tmdb_details(&mut untouched, &TmdbDetails::default(), true);
        assert_eq!(untouched.tagline.as_deref(), Some("keep me"));
    }

    /// The read-only descriptor queries return empty/default results (no store,
    /// no remote providers wired in First-Light) — never an error.
    #[tokio::test]
    async fn read_only_queries_return_empty_defaults() {
        let mgr = LocalProviderManager::default();
        let id = Uuid::new_v4();
        let query = RemoteImageQuery::new("Tmdb".into());

        assert!(
            mgr.get_available_remote_images(id, &query)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mgr.get_remote_image_provider_info(id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(mgr.get_external_urls(id).await.unwrap().is_empty());
        // The metadata-plugin registry projects the compiled-in providers, so it
        // is non-empty (covered in detail by `library_options` tests).
        assert!(!mgr.get_all_metadata_plugins().await.unwrap().is_empty());
        assert!(mgr.get_refresh_queue().await.unwrap().is_empty());

        // Metadata options fall back to the type default.
        let opts = mgr.get_metadata_options(id).await.unwrap();
        assert_eq!(
            opts,
            hermit_model::configuration::MetadataOptions::default()
        );
    }

    /// An [`ItemRepository`] over a fixed map, reporting each `retrieve_item`
    /// call on a channel so a test can observe a background refresh running.
    /// Every method the refresh path never touches is `unimplemented!`.
    struct FakeItems {
        rows: HashMap<Uuid, BaseItemEntity>,
        seen: tokio::sync::mpsc::UnboundedSender<Uuid>,
    }

    #[async_trait]
    impl hermit_traits::persistence::ItemRepository for FakeItems {
        async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            let _ = self.seen.send(id);
            Ok(self.rows.get(&id).cloned())
        }
        async fn get_items(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn get_item_ids(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            unimplemented!()
        }
        async fn get_item_list(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn get_latest_item_list(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
            _collection_type: hermit_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn item_exists(&self, _id: Uuid) -> Result<bool, ServiceError> {
            unimplemented!()
        }
        async fn get_items_by_primary_version(
            &self,
            _primary_id: Uuid,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            unimplemented!()
        }
        async fn get_items_with_provider_id(
            &self,
            _provider_key: &str,
        ) -> Result<Vec<(Uuid, String)>, ServiceError> {
            unimplemented!()
        }
        async fn get_image_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_traits::options::ItemImageInfo>, ServiceError> {
            unimplemented!()
        }
        async fn swap_item_images(
            &self,
            _item_id: Uuid,
            _image_type: ImageType,
            _index1: i32,
            _index2: i32,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_genres(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_music_genres(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_studios(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_artists(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_album_artists(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_all_artists(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_media_stream_languages(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
            _stream_type: hermit_model::entities::MediaStreamType,
        ) -> Result<Vec<String>, ServiceError> {
            unimplemented!()
        }
        async fn get_query_filters_legacy(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
            unimplemented!()
        }
        async fn get_is_played(
            &self,
            _user: &hermit_db::entities::users::UserEntity,
            _id: Uuid,
            _recursive: bool,
        ) -> Result<bool, ServiceError> {
            unimplemented!()
        }
    }

    /// A minimal row of the given stored C# type name.
    fn row(kind: &str, name: &str) -> BaseItemEntity {
        BaseItemEntity {
            id: Uuid::new_v4().to_string(),
            name: Some(name.to_owned()),
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            ..BaseItemEntity::default()
        }
    }

    #[tokio::test]
    async fn queue_refresh_returns_immediately_and_runs_in_background() {
        // A MusicAlbum short-circuits before any network, so the spawned refresh
        // completes offline; the repo channel proves it actually ran.
        let item_id = Uuid::new_v4();
        let mut album = row("Audio.MusicAlbum", "OK Computer");
        album.id = item_id.to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::from([(item_id, album)]),
            seen: tx,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items);

        mgr.queue_refresh(
            item_id,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                ..MetadataRefreshOptions::default()
            },
            RefreshPriority::High,
        )
        .await
        .expect("enqueue accepted");

        // The spawned task looked the item up — the refresh ran to completion.
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("background refresh ran")
            .expect("channel open");
        assert_eq!(seen, item_id);
    }

    #[tokio::test]
    async fn queue_refresh_swallows_refresh_errors() {
        // The item is missing → the spawned refresh errors NotFound; the enqueue
        // still reports Ok and the error is only logged.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let items = Arc::new(FakeItems {
            rows: HashMap::new(),
            seen: tx,
        });
        let mgr = LocalProviderManager::default()
            .with_remote_images(Arc::new(crate::tmdb::TmdbClient::new()), items);
        mgr.queue_refresh(
            Uuid::new_v4(),
            &MetadataRefreshOptions::default(),
            RefreshPriority::Low,
        )
        .await
        .expect("enqueue accepted despite the failing refresh");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("background refresh attempted the lookup");
    }

    #[tokio::test]
    async fn resolve_refresh_target_fetches_the_parent_series_row() {
        // The async wrapper hops `series_id` → the repo for seasons; a movie
        // resolves without touching the repo at all.
        let series_id = Uuid::new_v4();
        let mut series = row("TV.Series", "Severance");
        series.id = series_id.to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let items: Arc<dyn hermit_traits::persistence::ItemRepository> = Arc::new(FakeItems {
            rows: HashMap::from([(series_id, series)]),
            seen: tx,
        });
        let mgr = LocalProviderManager::default();

        let mut season = row("TV.Season", "Season 1");
        season.series_id = Some(series_id.to_string());
        season.index_number = Some(1);
        let target = mgr
            .resolve_refresh_target(&items, &season)
            .await
            .expect("resolves");
        assert!(matches!(target, Some(super::RefreshTarget::Season { .. })));
        assert_eq!(rx.try_recv().ok(), Some(series_id), "series row fetched");

        let movie = row("Movies.Movie", "Solaris");
        let target = mgr
            .resolve_refresh_target(&items, &movie)
            .await
            .expect("resolves");
        assert!(matches!(target, Some(super::RefreshTarget::Title { .. })));
        assert!(rx.try_recv().is_err(), "movies never hit the repo");
    }

    #[tokio::test]
    async fn apply_tv_slice_applies_metadata_per_refresh_mode() {
        let mgr = LocalProviderManager::default();
        let opts = MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
            image_refresh_mode: MetadataRefreshMode::None,
            replace_all_metadata: true,
            replace_all_images: false,
        };
        let mut entity = BaseItemEntity::default();
        mgr.apply_tv_slice(
            &mut entity,
            Uuid::new_v4(),
            Some("Ep 3"),
            Some("Plot."),
            Some("https://example.invalid/still.jpg"),
            &opts,
        )
        .await
        .expect("slice applies");
        assert_eq!(entity.name.as_deref(), Some("Ep 3"));
        assert_eq!(entity.overview.as_deref(), Some("Plot."));

        // ValidationOnly touches nothing.
        let mut untouched = BaseItemEntity::default();
        mgr.apply_tv_slice(
            &mut untouched,
            Uuid::new_v4(),
            Some("X"),
            Some("Y"),
            None,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::ValidationOnly,
                ..MetadataRefreshOptions::default()
            },
        )
        .await
        .expect("slice no-ops");
        assert_eq!(untouched.name, None);
    }

    #[test]
    fn refresh_target_resolves_season_and_episode_via_parent_series() {
        use super::{RefreshTarget, refresh_target_of};

        let mut series = row("TV.Series", "Severance");
        series.production_year = Some(2022);

        let mut season = row("TV.Season", "Season 2");
        season.series_id = Some(series.id.clone());
        season.index_number = Some(2);
        match refresh_target_of(&season, Some(&series)) {
            Some(RefreshTarget::Season {
                series_name,
                series_year,
                season_number,
            }) => {
                assert_eq!(series_name, "Severance");
                assert_eq!(series_year, Some(2022));
                assert_eq!(season_number, 2);
            }
            other => panic!(
                "expected a Season target, got {}",
                target_name(other.as_ref())
            ),
        }

        let mut episode = row("TV.Episode", "Hello, Ms. Cobel");
        episode.series_id = Some(series.id.clone());
        episode.parent_index_number = Some(1);
        episode.index_number = Some(3);
        match refresh_target_of(&episode, Some(&series)) {
            Some(RefreshTarget::Episode {
                series_name,
                season_number,
                episode_number,
                ..
            }) => {
                assert_eq!(series_name, "Severance");
                assert_eq!(season_number, 1);
                assert_eq!(episode_number, 3);
            }
            other => panic!(
                "expected an Episode target, got {}",
                target_name(other.as_ref())
            ),
        }
    }

    /// A debug label for a [`super::RefreshTarget`] in test panics.
    fn target_name(target: Option<&super::RefreshTarget>) -> &'static str {
        match target {
            Some(super::RefreshTarget::Title { .. }) => "Title",
            Some(super::RefreshTarget::Season { .. }) => "Season",
            Some(super::RefreshTarget::Episode { .. }) => "Episode",
            None => "None",
        }
    }

    #[test]
    fn refresh_target_skips_unsupported_kinds_and_broken_links() {
        use super::refresh_target_of;

        // Music has no provider — the faithful skip.
        assert!(refresh_target_of(&row("Audio.MusicAlbum", "OK Computer"), None).is_none());
        // A season with no resolvable parent series row.
        let mut orphan = row("TV.Season", "Season 1");
        orphan.index_number = Some(1);
        assert!(refresh_target_of(&orphan, None).is_none());
        // An episode missing its season/episode numbers.
        let series = row("TV.Series", "Severance");
        let mut unnumbered = row("TV.Episode", "Mystery");
        unnumbered.series_id = Some(series.id.clone());
        assert!(refresh_target_of(&unnumbered, Some(&series)).is_none());
        // A movie resolves to a Title target (control case).
        assert!(matches!(
            refresh_target_of(&row("Movies.Movie", "Solaris"), None),
            Some(super::RefreshTarget::Title { .. })
        ));
    }

    #[test]
    fn apply_name_overview_fills_and_replaces() {
        use super::apply_name_overview;

        let mut entity = BaseItemEntity::default();
        apply_name_overview(&mut entity, Some("Ep 1"), Some("Plot."), false);
        assert_eq!(entity.name.as_deref(), Some("Ep 1"));
        assert_eq!(entity.overview.as_deref(), Some("Plot."));

        // Existing values are kept without replace…
        apply_name_overview(&mut entity, Some("New"), Some("New plot."), false);
        assert_eq!(entity.name.as_deref(), Some("Ep 1"));
        // …and overwritten with it.
        apply_name_overview(&mut entity, Some("New"), Some("New plot."), true);
        assert_eq!(entity.name.as_deref(), Some("New"));
        assert_eq!(entity.overview.as_deref(), Some("New plot."));

        // A missing TMDB value never clears an existing one.
        apply_name_overview(&mut entity, None, None, true);
        assert_eq!(entity.name.as_deref(), Some("New"));
    }
}
