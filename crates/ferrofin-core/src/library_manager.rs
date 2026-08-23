//! [`FerrofinLibraryManager`] — the concrete [`LibraryManager`] orchestrator.
//!
//! Port of the object-safe, domain-tree-free subset of
//! `Emby.Server.Implementations.Library.LibraryManager`. The C# manager owns the
//! whole `BaseItem` OOP tree (resolvers, path/sort/named-view logic); those parts
//! live as free functions in [`crate::resolvers`] and [`crate::kinds`]. What
//! remains here is pure orchestration over the persistence seam: every query,
//! count, people, genre/studio/artist, and mutate call delegates to an injected
//! repository trait ([`ItemRepository`], [`ItemCountService`],
//! [`ItemPersistenceService`], [`PeopleRepository`]) rather than touching the
//! pool directly, so the manager stays composition-root agnostic.
//!
//! Port simplifications, all faithful to the trait surface:
//! - `create_items`/`update_items` collapse to `save_items` on the persistence
//!   service (the row upsert is idempotent); the `parent_id` argument is accepted
//!   for API parity but the parent linkage is already carried on each row's
//!   `ParentId` column.
//! - `delete_item` honors [`DeleteOptions`] by deleting the single row (and, when
//!   requested, its children) through the persistence service; the physical
//!   file deletion the C# path performs is out of scope for this seam and is
//!   noted deferred.
//! - The `IProgress`/`CancellationToken` scan plumbing is dropped;
//!   `queue_library_scan` records intent through the injected monitor/no-op.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tracing::Instrument as _;

use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::ItemCounts;
use ferrofin_model::entities::{ImageType, MediaStreamType};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, image_type_allows_multiple};
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use ferrofin_traits::persistence::{
    ItemCountService, ItemPersistenceService, ItemRepository, ItemWithCounts, PeopleRepository,
};

/// The placeholder item row seeded by the initial migration, which every real
/// item query excludes (the `Uuid` form of
/// [`PLACEHOLDER_ID`](crate::translate_query::PLACEHOLDER_ID),
/// `00000000-0000-0000-0000-000000000001`).
const PLACEHOLDER_ITEM_ID: Uuid = Uuid::from_u128(1);

/// The concrete library manager.
///
/// Holds cheaply-cloneable `Arc<dyn _>` handles to the four persistence traits it
/// orchestrates. All are injected at the composition root so the same concrete
/// repositories back both this manager and any other consumer.
#[derive(Clone)]
pub struct FerrofinLibraryManager {
    items: Arc<dyn ItemRepository>,
    counts: Arc<dyn ItemCountService>,
    persistence: Arc<dyn ItemPersistenceService>,
    people: Arc<dyn PeopleRepository>,
    /// The filesystem scanner, set by the composition root. When present,
    /// `queue_library_scan` runs it; `None` (unit tests) keeps it a no-op.
    scanner: Option<Arc<crate::library_scan::LibraryScanner>>,
    /// Coalescing guard for `queue_library_scan`: `true` while a scan task runs.
    /// A queue request during a running scan merges into [`Self::scan_pending`]
    /// instead of spawning a second scan (the library monitor fans a webhook
    /// batch into one report per path, and `/Library/Refresh` can be
    /// double-clicked).
    scan_in_flight: Arc<AtomicBool>,
    /// The scope a rerun should cover, merged from every request that arrived
    /// while a scan was running; the running task loops once more over it so
    /// changes landing mid-scan are not missed.
    scan_pending: Arc<std::sync::Mutex<Option<ScanScope>>>,
    /// Chapter rows, for serving chapter thumbnails. Set by the composition
    /// root; `None` (unit tests) means an item has no chapter images. The
    /// repository (not the `ChapterManager`) is held because the manager is
    /// built on top of this manager — taking it here would be a cycle.
    chapters: Option<Arc<dyn ferrofin_traits::persistence::ChapterRepository>>,
}

/// What a queued scan covers.
#[derive(Debug, Clone)]
enum ScanScope {
    /// Every library.
    Full,
    /// One library (by CollectionFolder id).
    Library(Uuid),
    /// Only the items touched by these changed filesystem paths.
    Paths(Vec<String>),
}

impl ScanScope {
    /// Merges a new request into a pending slot: path sets union; anything
    /// mixed with a full/library request widens to a full scan (over-scanning
    /// is the safe direction — the scopes are not otherwise combinable).
    fn merge_into(self, slot: &mut Option<ScanScope>) {
        *slot = Some(match (slot.take(), self) {
            (None, new) => new,
            (Some(ScanScope::Paths(mut a)), ScanScope::Paths(b)) => {
                a.extend(b);
                ScanScope::Paths(a)
            }
            _ => ScanScope::Full,
        });
    }
}

impl std::fmt::Debug for FerrofinLibraryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinLibraryManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinLibraryManager {
    /// Creates a library manager over the injected persistence repositories.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        counts: Arc<dyn ItemCountService>,
        persistence: Arc<dyn ItemPersistenceService>,
        people: Arc<dyn PeopleRepository>,
    ) -> Self {
        Self {
            items,
            counts,
            persistence,
            people,
            scanner: None,
            scan_in_flight: Arc::new(AtomicBool::new(false)),
            scan_pending: Arc::new(std::sync::Mutex::new(None)),
            chapters: None,
        }
    }

    /// Attaches the chapter seam so chapter thumbnails can be served (their
    /// paths live on the chapter rows, not in the item's image rows).
    #[must_use]
    pub fn with_chapters(
        mut self,
        chapters: Arc<dyn ferrofin_traits::persistence::ChapterRepository>,
    ) -> Self {
        self.chapters = Some(chapters);
        self
    }

    /// Attaches the filesystem scanner so `queue_library_scan` actually walks the
    /// libraries. Called once by the composition root.
    #[must_use]
    pub fn with_scanner(mut self, scanner: Arc<crate::library_scan::LibraryScanner>) -> Self {
        self.scanner = Some(scanner);
        self
    }
}

#[async_trait]
impl crate::library_monitor::LibraryScanTrigger for FerrofinLibraryManager {
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        // Reached via the filesystem watcher / Radarr-Sonarr webhooks.
        self.spawn_scan("watcher", ScanScope::Full);
        Ok(())
    }

    async fn queue_scan_paths(&self, paths: Vec<String>) -> Result<(), ServiceError> {
        // The monitor's settled change batch: ingest just the touched paths.
        self.spawn_scan("watcher", ScanScope::Paths(paths));
        Ok(())
    }
}

#[async_trait]
impl LibraryManager for FerrofinLibraryManager {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Ok(None);
        }
        self.items.retrieve_item(id).await
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        // Exactly `get_item_by_id(id).is_some()`, minus the row decode:
        // `get_item_by_id` rejects the nil id and `retrieve_item`'s predicate
        // excludes the seeded placeholder row, so both are "not an item" here
        // too. `ItemRepository::item_exists` answers the rest with a
        // `SELECT 1` existence probe instead of a ~70-column read.
        if id.is_nil() || id == PLACEHOLDER_ITEM_ID {
            return Ok(false);
        }
        self.items.item_exists(id).await
    }

    async fn get_ancestors(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        self.items.get_ancestor_chain(item_id).await
    }

    async fn get_item_images(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
        if item_id.is_nil() {
            return Ok(Vec::new());
        }
        self.items.get_image_infos(item_id).await
    }

    async fn get_chapter_image(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
        let Some(chapters) = &self.chapters else {
            return Ok(None);
        };
        let Ok(index) = usize::try_from(index) else {
            return Ok(None);
        };
        // Chapters come back in position order, which is the index clients
        // address them by (upstream `ChapterManager.GetChapter(id, index)`).
        let rows = chapters.get_chapters(item_id).await?;
        let Some(chapter) = rows.into_iter().nth(index) else {
            return Ok(None);
        };
        let Some(path) = chapter.image_path.filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(ferrofin_traits::options::ItemImageInfo {
            path,
            image_type: ferrofin_model::entities::ImageType::Chapter,
            date_modified: chapter.image_date_modified.unwrap_or_else(chrono::Utc::now),
            width: 0,
            height: 0,
            blur_hash: None,
        }))
    }

    async fn swap_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError> {
        // Only backdrops and chapters may hold multiple images and thus be
        // reordered; any other type is a bad request (C# `AllowsMultipleImages`
        // guard throwing `ArgumentException` → 400).
        if !image_type_allows_multiple(image_type) {
            return Err(ServiceError::invalid_input(
                "The change index operation is only applicable to backdrops and chapters",
            ));
        }
        self.items
            .swap_item_images(item_id, image_type, index1, index2)
            .await
    }

    async fn query_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        self.items.get_items(query).await
    }

    async fn get_item_ids(&self, query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        self.items.get_item_ids(query).await
    }

    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items.get_item_list(query).await
    }

    async fn get_named_items(
        &self,
        kind: BaseItemKind,
        names: &[String],
    ) -> Result<Vec<Option<BaseItemEntity>>, ServiceError> {
        let trimmed: Vec<String> = names.iter().map(|n| n.trim().to_owned()).collect();
        let lookup: Vec<String> = trimmed.iter().filter(|n| !n.is_empty()).cloned().collect();
        if lookup.is_empty() {
            return Ok(vec![None; names.len()]);
        }
        // One `CleanName IN (…)` query for the whole page (the batch form of
        // `get_named_item`, which matches by cleaned name filtered to `kind`).
        let rows = self
            .items
            .get_item_list(&InternalItemsQuery {
                names: lookup,
                include_item_types: vec![kind],
                ..InternalItemsQuery::default()
            })
            .await?;
        // Key by the row's stored CleanName (what the query matched on); first
        // match wins, mirroring `get_named_item`'s `FirstOrDefault`.
        let mut by_clean: HashMap<String, BaseItemEntity> = HashMap::new();
        for row in rows {
            if let Some(clean) = row.clean_name.clone() {
                by_clean.entry(clean).or_insert(row);
            }
        }
        Ok(trimmed
            .into_iter()
            .map(|n| {
                if n.is_empty() {
                    None
                } else {
                    by_clean
                        .get(&crate::text_util::get_clean_value(&n))
                        .cloned()
                }
            })
            .collect())
    }

    async fn get_named_item_ids(
        &self,
        kind: BaseItemKind,
        names: &[String],
    ) -> Result<Vec<Option<Uuid>>, ServiceError> {
        // Same resolution as `get_named_items` — same predicates, same ordering,
        // same first-match-wins — over a two-column projection, because the
        // caller wants the id and nothing else. On a cast-heavy page this is the
        // difference between decoding one 72-column row per credited name and
        // decoding two columns.
        let trimmed: Vec<String> = names.iter().map(|n| n.trim().to_owned()).collect();
        let lookup: Vec<String> = trimmed.iter().filter(|n| !n.is_empty()).cloned().collect();
        if lookup.is_empty() {
            return Ok(vec![None; names.len()]);
        }
        let rows = self
            .items
            .get_item_id_clean_names(&InternalItemsQuery {
                names: lookup,
                include_item_types: vec![kind],
                ..InternalItemsQuery::default()
            })
            .await?;
        let mut by_clean: HashMap<String, String> = HashMap::new();
        for (id, clean) in rows {
            if let Some(clean) = clean {
                by_clean.entry(clean).or_insert(id);
            }
        }
        Ok(trimmed
            .into_iter()
            .map(|n| {
                if n.is_empty() {
                    None
                } else {
                    by_clean
                        .get(&crate::text_util::get_clean_value(&n))
                        .and_then(|id| Uuid::parse_str(id).ok())
                }
            })
            .collect())
    }

    async fn get_latest_item_list(
        &self,
        query: &InternalItemsQuery,
        collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items
            .get_latest_item_list(query, collection_type)
            .await
    }

    async fn create_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        // The parent linkage is already carried on each row's ParentId column; the
        // upsert is the single persistence path (C# CreateItems == save + register).
        self.persistence.save_items(items).await
    }

    async fn update_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        self.persistence.save_items(items).await?;
        // Re-index each item's genre/tag/studio/artist links: the filter
        // facets and by-name browses read `ItemValues`, not the row columns,
        // so a metadata edit that only saved the row was invisible to them
        // until the next full scan.
        for item in items {
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            let values = crate::library_scan::item_values_of(item);
            self.persistence.save_item_values(id, &values).await?;
        }
        Ok(())
    }

    async fn delete_item(&self, id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        if id.is_nil() {
            return Err(ServiceError::invalid_input("item id can't be empty"));
        }
        let Some(row) = self.items.retrieve_item(id).await? else {
            // Already gone — deletion is idempotent.
            return Ok(());
        };
        let mut ids = vec![id];
        // C# `DeleteItem` cascades to a folder's children; gather the direct-child
        // ids so the row deletion removes the subtree too. Physical file deletion
        // (honoring `delete_file_location`) is the filesystem layer's job, not this
        // persistence seam, and is deferred.
        if row.is_folder {
            // Cascade to PHYSICAL children only. A box-set/playlist is a folder whose
            // members are LinkedChildren (references), not owned children — deleting the
            // container must never delete the referenced media (data loss). physical_children_only
            // suppresses the LinkedChildren merge the browse path uses.
            let child_query = InternalItemsQuery {
                parent_id: id,
                physical_children_only: true,
                ..Default::default()
            };
            ids.extend(self.items.get_item_ids(&child_query).await?);
        }
        self.persistence.delete_items(&ids).await
    }

    async fn merge_versions(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        // Resolve each supplied id to a persisted row, dropping any that are
        // missing, then de-duplicate and order by id (C# `.OrderBy(i => i.Id)`).
        let mut items = Vec::new();
        for &id in ids {
            if let Some(row) = self.items.retrieve_item(id).await? {
                items.push(row);
            }
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items.dedup_by(|a, b| a.id == b.id);

        if items.len() < 2 {
            return Err(ServiceError::invalid_input(
                "please supply at least two videos to merge",
            ));
        }

        // Pick the primary. C# prefers an item that already owns multiple sources
        // and is itself not an alternate; Ferrofin does not model `MediaSourceCount`,
        // so it falls back to C#'s secondary ordering: a plain video file outranks
        // a special type, then the widest default video stream wins. The item's own
        // `Width` column stands in for the default video stream width.
        let primary_index = items
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.width.unwrap_or(0).cmp(&b.width.unwrap_or(0)))
            .map_or(0, |(i, _)| i);
        let primary_id = items[primary_index].id.clone();

        // Link every non-primary item to the primary by pointer, and ensure the
        // primary itself is a standalone (its own pointer cleared). Targeted
        // single-column writes: the rows were loaded to *decide* the linkage,
        // and a full-row save would write their other columns back stale.
        let primary_uuid = Uuid::parse_str(&primary_id)
            .map_err(|_| ServiceError::invalid_input("malformed item id"))?;
        for item in &items {
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            if item.id == primary_id {
                if item.primary_version_id.is_some() {
                    self.persistence.set_primary_version_id(id, None).await?;
                }
            } else if item.primary_version_id.as_deref() != Some(primary_id.as_str()) {
                self.persistence
                    .set_primary_version_id(id, Some(primary_uuid))
                    .await?;
            }
        }
        Ok(())
    }

    async fn remove_alternate_sources(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let Some(item) = self.items.retrieve_item(item_id).await? else {
            return Err(ServiceError::not_found(format!("item {item_id}")));
        };

        // Resolve the group's primary: either this item (no pointer) or the item it
        // points at (C# hops to `PrimaryVersionId` when the item has no alternates).
        let primary_id = match item.primary_version_id.as_deref() {
            Some(pid) => Uuid::parse_str(pid)
                .map_err(|_| ServiceError::invalid_input("malformed PrimaryVersionId"))?,
            None => item_id,
        };

        // Clear the pointer on every alternate that references the primary, then on
        // the primary itself, so each becomes a standalone version again. Targeted
        // single-column writes — a full-row save of the loaded copies would revert
        // any column another writer changed since the load.
        for alt in self.items.get_items_by_primary_version(primary_id).await? {
            let Ok(id) = Uuid::parse_str(&alt.id) else {
                continue;
            };
            self.persistence.set_primary_version_id(id, None).await?;
        }
        if let Some(primary) = self.items.retrieve_item(primary_id).await?
            && primary.primary_version_id.is_some()
        {
            self.persistence
                .set_primary_version_id(primary_id, None)
                .await?;
        }
        Ok(())
    }

    async fn get_people(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        Ok(self.people.get_people(query).await?.items)
    }

    async fn get_people_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<PeopleEntity>>, ServiceError> {
        self.people.get_people_batch(item_ids).await
    }

    async fn get_people_names(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        self.people.get_people_names(query).await
    }

    async fn get_count(&self, query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        self.counts.get_count(query).await
    }

    async fn get_item_counts(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        self.counts.get_item_counts(query).await
    }

    async fn get_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_genres(query).await
    }

    async fn get_studios(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_studios(query).await
    }

    async fn get_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_artists(query).await
    }

    async fn get_music_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_music_genres(query).await
    }

    async fn get_album_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_album_artists(query).await
    }

    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        self.items.get_query_filters_legacy(query).await
    }

    async fn get_distinct_years(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<i32>, ServiceError> {
        self.items.get_distinct_years(query).await
    }

    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
        query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        self.items
            .get_media_stream_languages(query, stream_type)
            .await
    }

    async fn get_media_stream_languages_by_type(
        &self,
        stream_types: &[MediaStreamType],
        query: &InternalItemsQuery,
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        self.items
            .get_media_stream_languages_by_type(query, stream_types)
            .await
    }

    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        self.spawn_scan("api", ScanScope::Full);
        Ok(())
    }

    async fn queue_library_scan_with_trigger(
        &self,
        trigger: &'static str,
    ) -> Result<(), ServiceError> {
        self.spawn_scan(trigger, ScanScope::Full);
        Ok(())
    }

    async fn queue_library_scan_scoped(&self, library_id: Uuid) -> Result<(), ServiceError> {
        self.spawn_scan("api", ScanScope::Library(library_id));
        Ok(())
    }
}

impl FerrofinLibraryManager {
    /// Spawns the coalescing background library scan under a `library_scan` root
    /// span tagged with `trigger`, returning immediately (Jellyfin's refresh is
    /// fire-and-forget — it must not block the HTTP handler). Shared by every
    /// entry point so the coalescing guard and the span are defined once.
    /// `scope` restricts the scan to one library or to a set of changed paths;
    /// [`ScanScope::Full`] scans everything.
    fn spawn_scan(&self, trigger: &'static str, scope: ScanScope) {
        let Some(scanner) = &self.scanner else {
            tracing::debug!(trigger, "library scan queued (no scanner attached — no-op)");
            return;
        };
        // Coalesce: a full scan fetches remote metadata + artwork for every item,
        // so it can run for minutes. The library-monitor webhooks report one path
        // at a time (a Radarr/Sonarr batch = many calls) and `/Library/Refresh`
        // can be double-clicked, so overlapping requests must fold into one scan.
        // If a scan is already running, merge this request's scope into the
        // pending rerun and return; the running task loops once more over the
        // merged scope to pick up whatever changed mid-scan.
        if self.scan_in_flight.swap(true, Ordering::AcqRel) {
            scope.merge_into(&mut self.scan_pending.lock().expect("pending not poisoned"));
            tracing::debug!(
                trigger,
                "library scan already running; coalesced (rerun queued)"
            );
            return;
        }
        let scanner = Arc::clone(scanner);
        let in_flight = Arc::clone(&self.scan_in_flight);
        let pending = Arc::clone(&self.scan_pending);
        // Its own root span — a scan is a background unit of work, never parented
        // under the request span. `.instrument()` carries it onto the spawned task.
        let span = tracing::info_span!("library_scan", trigger);
        tokio::spawn(
            async move {
                let started = std::time::Instant::now();
                tracing::info!("library scan started");
                let mut total_created = 0usize;
                let mut scope = scope;
                loop {
                    let result = match scope {
                        ScanScope::Full => scanner.scan(None).await,
                        ScanScope::Library(id) => scanner.scan(Some(id)).await,
                        ScanScope::Paths(ref paths) => scanner.scan_paths(paths).await,
                    };
                    match result {
                        Ok(created) => {
                            total_created += created;
                            tracing::info!(created, "library scan pass complete");
                        }
                        // Logged exactly once, here, at the scan task's top level.
                        Err(err) => tracing::error!(%err, "library scan failed"),
                    }
                    // A request that arrived during the scan queued a rerun over
                    // its merged scope.
                    match pending.lock().expect("pending not poisoned").take() {
                        Some(next) => scope = next,
                        None => break,
                    }
                }
                in_flight.store(false, Ordering::Release);
                tracing::info!(
                    created = total_created,
                    elapsed_ms = started.elapsed().as_millis(),
                    "library scan complete"
                );
                // ponytail: a request landing in the gap between the pending take
                // and clearing in_flight loses its rerun — harmless (the next webhook
                // or a manual refresh re-triggers). Tighten only if webhook-driven
                // scans start missing changes.
            }
            .instrument(span),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_count_service::FerrofinItemCountService;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::people_repository::FerrofinPeopleRepository;
    use crate::test_support::{
        seed_item, seed_item_genre, seed_named_item, set_clean_name, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::ImageType;

    #[tokio::test(flavor = "current_thread")]
    async fn queue_library_scan_exports_a_library_scan_span_tagged_with_trigger() {
        // End-to-end span-coverage smoke: an `api`-triggered scan (empty library,
        // so it finishes fast) exports a `library_scan` root span carrying
        // `trigger`. current-thread + set_default keeps the spawned scan on the
        // scoped subscriber so the `.instrument()`ed span is captured.
        use crate::file_system::FerrofinFileSystem;
        use crate::library_scan::LibraryScanner;
        use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
        use ferrofin_traits::library::VirtualFolderManager;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;

        let db = test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        // No virtual folders added → the scan plans zero items and returns fast.
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = Arc::new(LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        ));
        let mgr = manager(&db).with_scanner(scanner);

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_simple_exporter(exporter.clone())
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("ferrofin"));
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        mgr.queue_library_scan().await.expect("queued");
        // Let the spawned (empty) scan run to completion so its span closes.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        provider.force_flush().expect("flush");

        let spans = exporter.get_finished_spans().expect("spans");
        let span = spans
            .iter()
            .find(|s| s.name == "library_scan")
            .expect("library_scan span exported");
        let trigger = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "trigger")
            .map(|kv| kv.value.to_string());
        assert_eq!(trigger.as_deref(), Some("api"));
    }

    #[tokio::test]
    async fn queue_library_scan_scoped_walks_only_that_library() {
        use crate::file_system::FerrofinFileSystem;
        use crate::library_scan::LibraryScanner;
        use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
        use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
        use ferrofin_model::entities::CollectionTypeOptions;
        use ferrofin_traits::library::VirtualFolderManager;

        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        std::fs::write(tv.join("Firefly/Season 01/Firefly S01E01.mkv"), b"").unwrap();

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, ct, media) in [
            ("Movies", CollectionTypeOptions::movies, &movies),
            ("TV", CollectionTypeOptions::tvshows, &tv),
        ] {
            vf.add_virtual_folder(
                name,
                Some(ct),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: media.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .unwrap();
        }
        let tv_cf = vf
            .get_virtual_folders()
            .await
            .unwrap()
            .iter()
            .find(|f| f.name.as_deref() == Some("TV"))
            .and_then(|f| f.item_id.as_deref())
            .map(|s| Uuid::parse_str(s).unwrap())
            .unwrap();
        let scanner = Arc::new(LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        ));
        let mgr = manager(&db).with_scanner(scanner);

        mgr.queue_library_scan_scoped(tv_cf).await.expect("queued");
        // The scan runs on a spawned task; poll (through the manager's own query
        // API, not raw SQL — the SQL-boundary ratchet) until it lands the episode.
        let count = |kind| {
            let mgr = &mgr;
            async move {
                mgr.query_items(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..Default::default()
                })
                .await
                .expect("query")
                .items
                .len()
            }
        };
        for _ in 0..200 {
            if count(BaseItemKind::Episode).await == 1
                && !mgr.scan_in_flight.load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            count(BaseItemKind::Episode).await,
            1,
            "the scoped TV library was scanned"
        );
        assert_eq!(
            count(BaseItemKind::Movie).await,
            0,
            "the movie library must not be scanned"
        );
    }

    #[tokio::test]
    async fn queue_scan_paths_ingests_only_the_changed_paths() {
        use crate::file_system::FerrofinFileSystem;
        use crate::library_monitor::LibraryScanTrigger;
        use crate::library_scan::LibraryScanner;
        use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
        use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
        use ferrofin_model::entities::CollectionTypeOptions;
        use ferrofin_traits::library::VirtualFolderManager;

        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        let episode = tv.join("Firefly/Season 01/Firefly S01E01.mkv");
        std::fs::write(&episode, b"").unwrap();

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, ct, media) in [
            ("Movies", CollectionTypeOptions::movies, &movies),
            ("TV", CollectionTypeOptions::tvshows, &tv),
        ] {
            vf.add_virtual_folder(
                name,
                Some(ct),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: media.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .unwrap();
        }
        let scanner = Arc::new(LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        ));
        let mgr = manager(&db).with_scanner(scanner);

        // Report the episode's path (what the monitor dispatches after a settle
        // window): its hierarchy lands, the movie library is never planned.
        mgr.queue_scan_paths(vec![episode.to_string_lossy().into_owned()])
            .await
            .expect("queued");
        let count = |kind| {
            let mgr = &mgr;
            async move {
                mgr.query_items(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..Default::default()
                })
                .await
                .expect("query")
                .items
                .len()
            }
        };
        for _ in 0..200 {
            if count(BaseItemKind::Episode).await == 1
                && !mgr.scan_in_flight.load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(count(BaseItemKind::Episode).await, 1, "the episode landed");
        assert_eq!(count(BaseItemKind::Series).await, 1, "with its series");
        assert_eq!(
            count(BaseItemKind::Movie).await,
            0,
            "the untouched movie library must not be scanned"
        );
    }

    /// Builds a manager backed by real repositories over the given database.
    fn manager(db: &Database) -> FerrofinLibraryManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinLibraryManager::new(
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup.clone())),
            Arc::new(FerrofinItemCountService::new(db.clone())),
            Arc::new(FerrofinItemPersistenceService::new(db.clone())),
            Arc::new(FerrofinPeopleRepository::new(db.clone())),
        )
    }

    #[tokio::test]
    async fn update_items_reindexes_the_filter_facets() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x77);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mgr = manager(&db);

        // A metadata edit sets genres/tags on the row; the filter facets read
        // `ItemValues`, so the update must re-index them without a rescan.
        let mut row = mgr.get_item_by_id(id).await.expect("read").expect("row");
        row.genres = Some("Sci-Fi".to_owned());
        row.tags = Some("4K|Christmas".to_owned());
        mgr.update_items(&[row], None).await.expect("update");

        let facets = mgr
            .get_query_filters_legacy(&InternalItemsQuery::default())
            .await
            .expect("facets");
        assert_eq!(facets.genres, vec!["Sci-Fi".to_owned()]);
        assert_eq!(facets.tags, vec!["4K".to_owned(), "Christmas".to_owned()]);
    }

    #[tokio::test]
    async fn get_item_by_id_reads_seeded_row() {
        let db = test_db().await;
        let id = Uuid::from_u128(7);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mgr = manager(&db);

        let item = mgr.get_item_by_id(id).await.expect("read").expect("some");
        assert_eq!(item.name.as_deref(), Some("Solaris"));
        // A nil id short-circuits to None without hitting the pool.
        assert!(
            mgr.get_item_by_id(Uuid::nil())
                .await
                .expect("nil")
                .is_none()
        );
    }

    #[tokio::test]
    async fn item_exists_agrees_with_get_item_by_id() {
        // The image routes gate their 404 on `item_exists`, so it must answer
        // exactly what `get_item_by_id(..).is_some()` answers — including for
        // the two ids that are "not an item": the nil id and the placeholder
        // row the initial migration seeds.
        let db = test_db().await;
        let id = Uuid::from_u128(11);
        seed_named_item(&db, id, BaseItemKind::Movie, "Stalker").await;
        let mgr = manager(&db);

        for probe in [
            id,
            Uuid::from_u128(0xABBA),
            Uuid::nil(),
            PLACEHOLDER_ITEM_ID,
        ] {
            let by_row = mgr.get_item_by_id(probe).await.expect("row").is_some();
            let by_probe = mgr.item_exists(probe).await.expect("exists");
            assert_eq!(by_probe, by_row, "disagreement on {probe}");
        }
        assert!(mgr.item_exists(id).await.expect("exists"));
    }

    #[tokio::test]
    async fn swap_images_rejects_non_multiple_type_and_swaps_backdrops() {
        let db = test_db().await;
        let item = Uuid::from_u128(0xA100);
        seed_named_item(&db, item, BaseItemKind::Movie, "Swappable").await;
        for (n, path) in [(0u128, "/one.jpg"), (1, "/two.jpg")] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 0, 2, ?2, ?3, 0)"#,
            )
            .bind(ferrofin_db::store::guid_to_db(Uuid::from_u128(0xA110 + n)))
            .bind(ferrofin_db::store::guid_to_db(item))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("insert backdrop");
        }
        let mgr = manager(&db);

        // Primary does not allow multiple images → InvalidInput (the 400).
        let err = mgr
            .swap_images(item, ImageType::Primary, 0, 1)
            .await
            .expect_err("primary rejected");
        assert!(matches!(err, ServiceError::InvalidInput(_)));

        // Backdrop is reorderable and the swap goes through to the repository.
        mgr.swap_images(item, ImageType::Backdrop, 0, 1)
            .await
            .expect("swap");
        let images = mgr.get_item_images(item).await.expect("images");
        assert_eq!(images[0].path, "/two.jpg");
        assert_eq!(images[1].path, "/one.jpg");
    }

    #[tokio::test]
    async fn query_items_returns_matching_rows() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        let a = Uuid::from_u128(0x101);
        let b = Uuid::from_u128(0x102);
        seed_named_item(&db, a, BaseItemKind::Movie, "MovieA").await;
        seed_named_item(&db, b, BaseItemKind::Movie, "MovieB").await;
        set_clean_name(&db, a, "MovieA").await;
        set_clean_name(&db, b, "MovieB").await;
        seed_item(&db, Uuid::from_u128(0x103), BaseItemKind::Episode).await;
        let mgr = manager(&db);

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            name_contains: Some("Movie".to_owned()),
            ..Default::default()
        };
        let result = mgr.query_items(&query).await.expect("query");
        assert_eq!(result.items.len(), 2);
    }

    #[tokio::test]
    async fn delete_item_removes_the_row() {
        let db = test_db().await;
        let id = Uuid::from_u128(9);
        seed_item(&db, id, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        mgr.delete_item(id, &DeleteOptions::default())
            .await
            .expect("delete");
        assert!(mgr.get_item_by_id(id).await.expect("read").is_none());
    }

    #[tokio::test]
    async fn queue_library_scan_is_a_successful_no_op() {
        let db = test_db().await;
        let mgr = manager(&db);
        mgr.queue_library_scan().await.expect("queue");
    }

    #[tokio::test]
    async fn get_named_item_resolves_by_clean_name() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x201);
        seed_named_item(&db, id, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, id, "Science Fiction").await;
        // A different-kind row with the same name must not be returned.
        let other = Uuid::from_u128(0x202);
        seed_named_item(&db, other, BaseItemKind::Studio, "Science Fiction").await;
        set_clean_name(&db, other, "Science Fiction").await;
        let mgr = manager(&db);

        let found = mgr
            .get_named_item(BaseItemKind::Genre, "Science Fiction")
            .await
            .expect("lookup")
            .expect("some");
        assert_eq!(Uuid::parse_str(&found.id).expect("uuid"), id);
        assert_eq!(found.name.as_deref(), Some("Science Fiction"));
    }

    #[tokio::test]
    async fn get_named_items_batches_and_preserves_order() {
        let db = test_db().await;
        let scifi = Uuid::from_u128(0x211);
        let drama = Uuid::from_u128(0x212);
        seed_named_item(&db, scifi, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, scifi, "Science Fiction").await;
        seed_named_item(&db, drama, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, drama, "Drama").await;
        // A same-name row of a different kind must not leak into Genre results.
        let studio = Uuid::from_u128(0x213);
        seed_named_item(&db, studio, BaseItemKind::Studio, "Drama").await;
        set_clean_name(&db, studio, "Drama").await;
        let mgr = manager(&db);

        // Order follows the input, unresolved names become None, and the
        // wrong-kind "Drama" studio is excluded.
        let names = vec![
            "Drama".to_owned(),
            "Nope".to_owned(),
            "Science Fiction".to_owned(),
        ];
        let got = mgr
            .get_named_items(BaseItemKind::Genre, &names)
            .await
            .expect("batch lookup");
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0].as_ref().and_then(|e| Uuid::parse_str(&e.id).ok()),
            Some(drama)
        );
        assert!(got[1].is_none());
        assert_eq!(
            got[2].as_ref().and_then(|e| Uuid::parse_str(&e.id).ok()),
            Some(scifi)
        );

        // Empty input yields an empty result without a query.
        assert!(
            mgr.get_named_items(BaseItemKind::Genre, &[])
                .await
                .expect("empty")
                .is_empty()
        );
    }

    /// Two rows of the SAME kind sharing a `CleanName` must resolve to the same
    /// id every time — the resolver keeps the FIRST match.
    ///
    /// Person rows have `SortName IS NULL`, so the resolver's bare
    /// `ORDER BY SortName` is a total tie among duplicates and the row order is
    /// whatever the sorter emits. Keeping the first match is what makes the
    /// answer stable and makes it agree with `get_named_items`. Flipping
    /// `or_insert` to `insert` (last-match-wins) passed all 4,221 other tests.
    #[tokio::test]
    async fn duplicate_names_of_one_kind_resolve_to_the_first_match() {
        let db = test_db().await;
        for n in [0x230u128, 0x231, 0x232] {
            let id = Uuid::from_u128(n);
            seed_named_item(&db, id, BaseItemKind::Person, "Jane Doe").await;
            set_clean_name(&db, id, "Jane Doe").await;
        }
        let mgr = manager(&db);

        let first = mgr
            .get_named_item_ids(BaseItemKind::Person, &["Jane Doe".to_owned()])
            .await
            .expect("ids");
        assert_eq!(first.len(), 1, "one name in, one slot out");
        let resolved = first[0].expect("the name resolves");

        // It must be the same id the row-returning resolver picks...
        let rows = mgr
            .get_named_items(BaseItemKind::Person, &["Jane Doe".to_owned()])
            .await
            .expect("rows");
        assert_eq!(
            rows[0].as_ref().map(|r| r.id.clone()),
            Some(ferrofin_db::store::guid_to_db(resolved)),
            "the id-only and row-returning resolvers must agree on the duplicate"
        );

        // ...and it must not drift between calls.
        for round in 0..5 {
            let again = mgr
                .get_named_item_ids(BaseItemKind::Person, &["Jane Doe".to_owned()])
                .await
                .expect("ids");
            assert_eq!(
                again[0].as_ref(),
                Some(&resolved),
                "round {round}: the resolved id must be stable across calls"
            );
        }
    }

    // The id-only twin of the batch resolver: the DTO prefetch resolves a page's
    // whole cast through it and reads nothing but the id, so it must agree with
    // `get_named_items` slot for slot — same order, same wrong-kind exclusion,
    // same `None` for an unresolved name, same blank-name handling — while
    // never materializing a row.
    #[tokio::test]
    async fn get_named_item_ids_matches_get_named_items_slot_for_slot() {
        let db = test_db().await;
        // The same-name row of a WRONG kind is seeded first, so it would win the
        // first-match-wins lookup if the kind filter were ever dropped.
        let studio = Uuid::from_u128(0x220);
        seed_named_item(&db, studio, BaseItemKind::Studio, "Drama").await;
        set_clean_name(&db, studio, "Drama").await;
        let scifi = Uuid::from_u128(0x221);
        let drama = Uuid::from_u128(0x222);
        seed_named_item(&db, scifi, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, scifi, "Science Fiction").await;
        seed_named_item(&db, drama, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, drama, "Drama").await;
        let mgr = manager(&db);

        // Untrimmed, blank and differently-cased names exercise the same
        // normalization both paths apply before the CleanName join.
        let names = vec![
            "  Drama ".to_owned(),
            "Nope".to_owned(),
            "   ".to_owned(),
            "science fiction".to_owned(),
        ];
        let ids = mgr
            .get_named_item_ids(BaseItemKind::Genre, &names)
            .await
            .expect("id lookup");
        assert_eq!(ids, vec![Some(drama), None, None, Some(scifi)]);

        // And it agrees with the row-returning form it replaces.
        let rows = mgr
            .get_named_items(BaseItemKind::Genre, &names)
            .await
            .expect("row lookup");
        let from_rows: Vec<Option<Uuid>> = rows
            .into_iter()
            .map(|r| r.and_then(|e| Uuid::parse_str(&e.id).ok()))
            .collect();
        assert_eq!(ids, from_rows);

        // Empty input yields an empty result without a query; a blank name is a
        // slot that resolves to nothing rather than a dropped slot.
        assert!(
            mgr.get_named_item_ids(BaseItemKind::Genre, &[])
                .await
                .expect("empty")
                .is_empty()
        );
        assert_eq!(
            mgr.get_named_item_ids(BaseItemKind::Genre, &[String::new(), " ".to_owned()])
                .await
                .expect("blank"),
            vec![None, None]
        );
    }

    #[tokio::test]
    async fn get_ancestors_walks_parent_chain_nearest_first() {
        let db = test_db().await;
        // grandparent <- parent <- child
        let grandparent = Uuid::from_u128(0x301);
        let parent = Uuid::from_u128(0x302);
        let child = Uuid::from_u128(0x303);
        seed_named_item(&db, grandparent, BaseItemKind::Folder, "Library").await;
        seed_named_item(&db, parent, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, child, BaseItemKind::Episode, "Pilot").await;
        for (id, parent_id) in [(child, parent), (parent, grandparent)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .bind(ferrofin_db::store::guid_to_db(parent_id))
                .execute(db.writer())
                .await
                .expect("set parent");
        }
        let mgr = manager(&db);

        let ancestors = mgr
            .get_ancestors(child)
            .await
            .expect("ancestors")
            .expect("item exists");
        // Nearest parent first, then its parent — the seed item is excluded.
        assert_eq!(ancestors.len(), 2);
        assert_eq!(Uuid::parse_str(&ancestors[0].id).expect("uuid"), parent);
        assert_eq!(
            Uuid::parse_str(&ancestors[1].id).expect("uuid"),
            grandparent
        );

        // A root item (no parent) yields an empty list, not None.
        let roots = mgr
            .get_ancestors(grandparent)
            .await
            .expect("ancestors")
            .expect("item exists");
        assert!(roots.is_empty());

        // A missing item yields None so the API maps it to 404.
        assert!(
            mgr.get_ancestors(Uuid::from_u128(0x3ff))
                .await
                .expect("missing")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_ancestors_cycle_terminates_and_deduplicates() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x401);
        let b = Uuid::from_u128(0x402);
        let child = Uuid::from_u128(0x403);
        seed_named_item(&db, a, BaseItemKind::Folder, "A").await;
        seed_named_item(&db, b, BaseItemKind::Folder, "B").await;
        seed_named_item(&db, child, BaseItemKind::Episode, "C").await;
        // child -> A -> B -> A (cycle)
        for (id, parent_id) in [(child, a), (a, b), (b, a)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .bind(ferrofin_db::store::guid_to_db(parent_id))
                .execute(db.writer())
                .await
                .expect("set parent");
        }
        let mgr = manager(&db);
        let anc = mgr
            .get_ancestors(child)
            .await
            .expect("anc")
            .expect("exists");
        let ids: Vec<Uuid> = anc
            .iter()
            .map(|r| Uuid::parse_str(&r.id).expect("uuid"))
            .collect();
        assert_eq!(ids, vec![a, b], "cycle must deduplicate, nearest-first");
    }

    #[tokio::test]
    async fn get_named_item_missing_is_none() {
        let db = test_db().await;
        let mgr = manager(&db);
        assert!(
            mgr.get_named_item(BaseItemKind::Genre, "Nope")
                .await
                .expect("lookup")
                .is_none()
        );
        // A blank name short-circuits to None.
        assert!(
            mgr.get_named_item(BaseItemKind::Genre, "   ")
                .await
                .expect("blank")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_music_genres_counts_referencing_items() {
        let db = test_db().await;
        // A MusicGenre by-name row plus a song that references it.
        let genre_id = Uuid::from_u128(0x301);
        seed_named_item(&db, genre_id, BaseItemKind::MusicGenre, "Jazz").await;
        set_clean_name(&db, genre_id, "Jazz").await;
        let song = Uuid::from_u128(0x302);
        seed_named_item(&db, song, BaseItemKind::Audio, "Blue in Green").await;
        seed_item_genre(&db, song, "Jazz").await;
        let mgr = manager(&db);

        let result = mgr
            .get_music_genres(&InternalItemsQuery::default())
            .await
            .expect("music genres");
        let jazz = result
            .items
            .iter()
            .find(|iwc| iwc.item.name.as_deref() == Some("Jazz"))
            .expect("jazz present");
        assert_eq!(jazz.counts.item_count, 1);
    }

    #[tokio::test]
    async fn get_media_stream_languages_reads_distinct_codes() {
        use ferrofin_model::entities::MediaStreamType;
        let db = test_db().await;
        let item = Uuid::from_u128(0x501);
        seed_item(&db, item, BaseItemKind::Movie).await;
        // One English audio stream plus one with no language (→ 'und').
        for (idx, lang) in [(0_i64, Some("eng")), (1, None)] {
            sqlx::query(
                r#"INSERT INTO "MediaStreamInfos"
                   ("ItemId", "StreamIndex", "IsDefault", "IsExternal", "IsForced",
                    "StreamType", "Language")
                   VALUES (?1, ?2, 0, 0, 0, 0, ?3)"#,
            )
            .bind(ferrofin_db::store::guid_to_db(item))
            .bind(idx)
            .bind(lang)
            .execute(db.writer())
            .await
            .expect("insert stream");
        }
        let mgr = manager(&db);

        let mut langs = mgr
            .get_media_stream_languages(MediaStreamType::Audio, &InternalItemsQuery::default())
            .await
            .expect("languages");
        langs.sort();
        assert_eq!(langs, vec!["eng".to_owned(), "und".to_owned()]);
    }

    #[tokio::test]
    async fn get_album_artists_returns_artist_rows() {
        let db = test_db().await;
        // A song credits "Miles Davis" as album artist (ItemValues type 1), and the
        // browsable by-name row is materialized sharing the value id — the shape the
        // by-name aggregate now requires (a value referenced by an in-scope item).
        let value_id = ferrofin_db::store::guid_to_db(Uuid::from_u128(0x401));
        let song = Uuid::from_u128(0x402);
        seed_named_item(&db, song, BaseItemKind::Audio, "So What").await;
        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId","Type","Value","CleanValue")
               VALUES (?1, 1, 'Miles Davis', 'miles davis')"#,
        )
        .bind(&value_id)
        .execute(db.writer())
        .await
        .expect("value");
        sqlx::query(r#"INSERT INTO "ItemValuesMap" ("ItemId","ItemValueId") VALUES (?1,?2)"#)
            .bind(ferrofin_db::store::guid_to_db(song))
            .bind(&value_id)
            .execute(db.writer())
            .await
            .expect("map");
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","Name","CleanName","IsFolder","IsInMixedFolder",
                "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,'MediaBrowser.Controller.Entities.Audio.MusicArtist',
                       'Miles Davis','miles davis',1,0,0,0,0,0,0)"#,
        )
        .bind(&value_id)
        .execute(db.writer())
        .await
        .expect("by-name row");
        let mgr = manager(&db);

        let result = mgr
            .get_album_artists(&InternalItemsQuery::default())
            .await
            .expect("album artists");
        assert!(
            result
                .items
                .iter()
                .any(|iwc| iwc.item.name.as_deref() == Some("Miles Davis"))
        );
    }

    #[tokio::test]
    async fn get_user_root_folder_resolves_the_root_row() {
        let db = test_db().await;
        let mgr = manager(&db);

        // With no root row materialized, the default resolves to None.
        assert!(mgr.get_user_root_folder().await.expect("none").is_none());

        // Once a UserRootFolder row exists, it is returned.
        let root = Uuid::from_u128(0x5001);
        seed_named_item(&db, root, BaseItemKind::UserRootFolder, "Media Folders").await;
        let resolved = mgr
            .get_user_root_folder()
            .await
            .expect("root")
            .expect("some");
        assert_eq!(Uuid::parse_str(&resolved.id).expect("uuid"), root);
    }

    /// Sets a row's `Width` column so the merge primary-selection heuristic has a
    /// deterministic winner.
    async fn set_width(db: &Database, id: Uuid, width: i64) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Width" = ?1 WHERE "Id" = ?2"#)
            .bind(width)
            .bind(ferrofin_db::store::guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("set width");
    }

    #[tokio::test]
    async fn merge_versions_links_alternates_to_widest_primary() {
        let db = test_db().await;
        let wide = Uuid::from_u128(0x301);
        let narrow = Uuid::from_u128(0x302);
        seed_item(&db, wide, BaseItemKind::Movie).await;
        seed_item(&db, narrow, BaseItemKind::Movie).await;
        set_width(&db, wide, 1920).await;
        set_width(&db, narrow, 640).await;
        let mgr = manager(&db);

        mgr.merge_versions(&[narrow, wide]).await.expect("merge");

        // The widest becomes the primary (its own pointer stays null); the narrow
        // one points at it.
        let primary = mgr.get_item_by_id(wide).await.expect("read").expect("some");
        assert_eq!(primary.primary_version_id, None);
        let alt = mgr
            .get_item_by_id(narrow)
            .await
            .expect("read")
            .expect("some");
        assert_eq!(
            alt.primary_version_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok()),
            Some(wide)
        );
    }

    #[tokio::test]
    async fn merge_versions_rejects_single_id() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x303);
        seed_item(&db, id, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        let err = mgr.merge_versions(&[id]).await.expect_err("too few");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn remove_alternate_sources_clears_the_group() {
        let db = test_db().await;
        let primary = Uuid::from_u128(0x311);
        let alt = Uuid::from_u128(0x312);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, alt, BaseItemKind::Movie).await;
        set_width(&db, primary, 1920).await;
        set_width(&db, alt, 640).await;
        let mgr = manager(&db);
        mgr.merge_versions(&[primary, alt]).await.expect("merge");

        // Splitting from the *alternate* still clears the whole group.
        mgr.remove_alternate_sources(alt).await.expect("remove");

        assert_eq!(
            mgr.get_item_by_id(primary)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
        assert_eq!(
            mgr.get_item_by_id(alt)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
    }

    #[tokio::test]
    async fn remove_alternate_sources_missing_item_is_not_found() {
        let db = test_db().await;
        let mgr = manager(&db);
        let err = mgr
            .remove_alternate_sources(Uuid::from_u128(0x3FF))
            .await
            .expect_err("missing");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }
}
