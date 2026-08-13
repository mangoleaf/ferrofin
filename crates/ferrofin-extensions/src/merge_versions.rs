//! The **Merge Versions** extension — a compiled-in port of
//! [`jellyfin-plugin-mergeversions`](https://github.com/danieladov/jellyfin-plugin-mergeversions)
//! (GUID `f21bbed8-3a97-4d8b-88b2-48aaa65427cb`), faithful to upstream 12.0
//! (`e6f58d6`).
//!
//! Groups multiple video files of the same movie/episode into one item with
//! selectable versions by pointing each alternate's `PrimaryVersionId` at a
//! primary. Ported surface, grouped here so the whole plugin lives in one
//! place:
//! - [`MergeVersionsService`] — the bulk scans (upstream
//!   `MergeVersionsManager`): movies grouped by `Tmdb` id, episodes by the
//!   provider-first merge key, with the config-driven excluded-locations and
//!   inactive-library eligibility filters and the transitive version-group
//!   expansion of `GetAllAlternateVersions`;
//! - [`MergeVersionsExtension`] — the `/Plugins` presentation, the vendored
//!   upstream settings page, and the two dashboard tasks (upstream
//!   `MergeMoviesTask`/`MergeEpisodesTask`, 24-hour interval);
//! - the four `/MergeVersions/*` routes stay in `ferrofin-api` (the thin HTTP
//!   seam) and reach [`MergeVersionsService`] through the
//!   [`MergeVersionsManager`] trait.
//!
//! Accepted divergences from the C# (see `docs/PLUGINS_UPSTREAM.md`):
//! - The episode merge key is scoped to the series *row*
//!   (`SeriesPresentationUniqueKey`), not the series name: upstream's
//!   name-scoped key merges episodes across the two series a show gets when it
//!   exists in two libraries (e.g. hot/cold storage tiers), hiding each
//!   alternate from its own series' episode list and skewing season counts.
//!   The bulk episode task also self-heals: links whose key no longer matches
//!   their primary's are unlinked and regrouped within their own series.
//! - Ferrofin models a version group solely by the `PrimaryVersionId` pointer;
//!   the upstream `OwnerId`/`LocalAlternateVersions`/`LinkedAlternateVersions`
//!   columns and the linked-child reroute are Jellyfin-internal representation
//!   with no Ferrofin equivalent.
//! - No `VideoType`/`Video3DFormat` columns exist, so primary selection cannot
//!   demote 3D/non-file videos — the widest wins.
//! - No `IndexNumberEnd` column exists, so that merge-key component is always
//!   empty (only multi-episode files could tell the difference).
//! - The upstream movie loops run their async merges inside `Parallel.ForEach`
//!   fire-and-forget lambdas (dropping errors); Ferrofin awaits sequentially.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_core::{PluginConfigPage, ScheduledTask, TaskProgress};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::merge_versions::{MergeProgress, MergeVersionsManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Extension, ExtensionContext};

/// The Merge Versions plugin's stable id — the **upstream plugin's GUID**, so
/// the vendored settings page (which calls `getPluginConfiguration` with this
/// exact id) and existing dashboards recognize it.
pub const EXTENSION_ID: Uuid = Uuid::from_u128(0xf21b_bed8_3a97_4d8b_88b2_48aa_a654_27cb);

/// The plugin configuration — upstream `PluginConfiguration`: the library
/// locations excluded from the bulk scans (the settings page's checkbox list).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MergeVersionsConfig {
    /// Absolute library locations whose items the bulk scans skip.
    pub locations_excluded: Vec<String>,
}

/// The `/Plugins` surface of the Merge Versions extension.
#[derive(Debug, Default, Clone, Copy)]
pub struct MergeVersionsExtension;

impl Extension for MergeVersionsExtension {
    fn id(&self) -> Uuid {
        EXTENSION_ID
    }

    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: EXTENSION_ID,
            name: "Merge Versions".to_owned(),
            // The upstream plugin version the port (and its vendored settings
            // page) tracks.
            version: "12.0.0.0".to_owned(),
            description: "Automatically groups multiple files of the same movie or episode \
                          into one item with selectable versions."
                .to_owned(),
            enabled: true,
            has_image: false,
            can_uninstall: false,
        }
    }

    fn default_config(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&MergeVersionsConfig::default())
            .unwrap_or_else(|_| b"{}".to_vec())
    }

    fn config_pages(&self) -> Vec<PluginConfigPage> {
        // The upstream plugin's own settings page (vendored by `build.rs`);
        // upstream's `GetPages` does not enable it in the main menu.
        vec![PluginConfigPage {
            name: "Merge Versions".to_owned(),
            bytes: include_bytes!(concat!(
                env!("OUT_DIR"),
                "/mergeversions/configurationpage.html"
            ))
            .to_vec(),
            enable_in_main_menu: false,
        }]
    }

    fn tasks(&self, cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
        vec![
            Arc::new(MergeMoviesTask {
                service: Arc::clone(&cx.merge_versions),
                plugins: Arc::clone(&cx.plugins),
            }),
            Arc::new(MergeEpisodesTask {
                service: Arc::clone(&cx.merge_versions),
                plugins: Arc::clone(&cx.plugins),
            }),
        ]
    }
}

/// One day as scheduler ticks (100 ns) — the upstream tasks' default interval.
const DAY_TICKS: i64 = 24 * 3600 * 10_000_000;

/// The merge tasks' shared default triggers: every 24 hours (upstream's
/// default) plus at startup. The startup trigger is a deliberate divergence
/// from upstream: the interval clock resets on every boot, so a server that
/// restarts more often than daily would otherwise never run the merge at all.
fn default_triggers() -> Vec<TaskTriggerInfo> {
    vec![
        TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(DAY_TICKS),
            ..TaskTriggerInfo::default()
        },
        TaskTriggerInfo {
            type_: TaskTriggerInfoType::StartupTrigger,
            ..TaskTriggerInfo::default()
        },
    ]
}

/// Whether the Merge Versions plugin is currently enabled (live toggle).
async fn plugin_enabled(plugins: &Arc<dyn PluginManager>) -> bool {
    matches!(
        plugins.get_plugin(EXTENSION_ID).await,
        Ok(Some(p)) if p.enabled
    )
}

/// The "Merge All Movies" dashboard task — upstream `MergeMoviesTask`.
struct MergeMoviesTask {
    service: Arc<dyn MergeVersionsManager>,
    plugins: Arc<dyn PluginManager>,
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for MergeMoviesTask {
    fn key(&self) -> &str {
        "MergeMoviesTask"
    }
    fn name(&self) -> &str {
        "Merge All Movies"
    }
    fn description(&self) -> &str {
        "Scans all libraries to merge repeated movies"
    }
    fn category(&self) -> &str {
        "Merge Versions"
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        default_triggers()
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // Gate on the plugin being enabled (live toggle — no restart needed).
        if !plugin_enabled(&self.plugins).await {
            tracing::debug!("merge versions disabled; skipping movie merge");
            return Ok(());
        }
        let report = |p: f64| progress.report(p);
        self.service.merge_movies(Some(&report)).await
    }
}

/// The "Merge All Episodes" dashboard task — upstream `MergeEpisodesTask`.
struct MergeEpisodesTask {
    service: Arc<dyn MergeVersionsManager>,
    plugins: Arc<dyn PluginManager>,
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for MergeEpisodesTask {
    fn key(&self) -> &str {
        "MergeEpisodesTask"
    }
    fn name(&self) -> &str {
        "Merge All Episodes"
    }
    fn description(&self) -> &str {
        "Merges all repeated episodes"
    }
    fn category(&self) -> &str {
        "Merge Versions"
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        default_triggers()
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        if !plugin_enabled(&self.plugins).await {
            tracing::debug!("merge versions disabled; skipping episode merge");
            return Ok(());
        }
        let report = |p: f64| progress.report(p);
        self.service.merge_episodes(Some(&report)).await
    }
}

/// The concrete [`MergeVersionsManager`] — port of the upstream
/// `MergeVersionsManager` over Ferrofin's persistence seams.
pub struct MergeVersionsService {
    items: Arc<dyn ItemRepository>,
    persistence: Arc<dyn ItemPersistenceService>,
    library: Arc<dyn LibraryManager>,
    virtual_folders: Arc<dyn VirtualFolderManager>,
    plugins: Arc<dyn PluginManager>,
}

impl MergeVersionsService {
    /// Builds the service over the item repository/persistence (read + link
    /// writes), the library manager (group unlinking), the virtual-folder
    /// manager (the inactive-library check), and the plugin manager (enabled
    /// flag + `LocationsExcluded` configuration, read live on every scan).
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        persistence: Arc<dyn ItemPersistenceService>,
        library: Arc<dyn LibraryManager>,
        virtual_folders: Arc<dyn VirtualFolderManager>,
        plugins: Arc<dyn PluginManager>,
    ) -> Self {
        Self {
            items,
            persistence,
            library,
            virtual_folders,
            plugins,
        }
    }

    /// Errors with [`ServiceError::NotFound`] while the plugin is disabled —
    /// the observable behavior of a Jellyfin server whose disabled plugin's
    /// controller is not registered (its routes 404).
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        if plugin_enabled(&self.plugins).await {
            Ok(())
        } else {
            Err(ServiceError::not_found(
                "the Merge Versions plugin is disabled",
            ))
        }
    }

    /// Loads the persisted configuration, falling back to defaults.
    async fn config(&self) -> MergeVersionsConfig {
        match self.plugins.get_plugin_configuration(EXTENSION_ID).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => MergeVersionsConfig::default(),
        }
    }

    /// Lists every non-virtual item of `kind` that passes the upstream
    /// `IsEligible` filters (upstream `GetMoviesFromLibrary` /
    /// `GetEpisodesFromLibrary`): not under an excluded location, and — for
    /// movies only — not in an inactive library (its parent folder must sit
    /// under some virtual-folder location).
    async fn eligible_items(
        &self,
        kind: BaseItemKind,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let excluded = self.config().await.locations_excluded;
        let folders = self.virtual_folders.get_virtual_folders().await?;
        let items = self
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![kind],
                is_virtual_item: Some(false),
                recursive: true,
                // The general query hides merged alternates (`PrimaryVersionId
                // IS NULL`); the bulk scans must see them — upstream's query
                // does — or already-linked versions are invisible to the
                // existing-primary probe, the movies' any-unmerged filter, and
                // the episode self-heal pass.
                include_owned_items: true,
                ..Default::default()
            })
            .await?;
        Ok(items
            .into_iter()
            .filter(|item| !in_excluded_location(&excluded, item.path.as_deref()))
            .filter(|item| {
                kind != BaseItemKind::Movie || !in_inactive_library(&folders, item.path.as_deref())
            })
            .collect())
    }

    /// Expands a set of items to the union of their existing version groups —
    /// port of `GetAllAlternateVersions`: from each item, follow its
    /// `PrimaryVersionId` pointer up and every row pointing at it down, to
    /// fixpoint. Merging into an already-merged item thus re-groups the whole
    /// family instead of nesting groups.
    async fn expand_group(
        &self,
        seed: &[BaseItemEntity],
    ) -> Result<HashMap<String, BaseItemEntity>, ServiceError> {
        let mut group: HashMap<String, BaseItemEntity> = HashMap::new();
        let mut pending: VecDeque<BaseItemEntity> = seed.iter().cloned().collect();
        while let Some(item) = pending.pop_front() {
            if group.contains_key(&item.id) {
                continue;
            }
            if let Some(pid) = item.primary_version_id.as_deref()
                && !group.contains_key(pid)
                && let Ok(pid) = Uuid::parse_str(pid)
                && let Some(primary) = self.items.retrieve_item(pid).await?
            {
                pending.push_back(primary);
            }
            if let Ok(id) = Uuid::parse_str(&item.id) {
                for alt in self.items.get_items_by_primary_version(id).await? {
                    if !group.contains_key(&alt.id) {
                        pending.push_back(alt);
                    }
                }
            }
            group.insert(item.id.clone(), item);
        }
        Ok(group)
    }

    /// Merges one duplicate group — port of the upstream private
    /// `MergeVersions`: resolve + order by id, expand to the full existing
    /// groups, pick the primary, and point every other member's
    /// `PrimaryVersionId` at it. Fewer than two resolvable items is a silent
    /// no-op (the C# returns), unlike the strict `POST /Videos/MergeVersions`
    /// route.
    async fn merge_group(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        let mut items = Vec::new();
        for &id in ids {
            if let Some(row) = self.items.retrieve_item(id).await? {
                items.push(row);
            }
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items.dedup_by(|a, b| a.id == b.id);
        if items.len() < 2 {
            return Ok(());
        }

        let group = self.expand_group(&items).await?;

        // Primary selection: the first supplied item (id order) that already
        // owns alternates and is not itself an alternate — the C#
        // `MediaSourceCount > 1 && !PrimaryVersionId.HasValue` probe — else
        // the widest (`Video3DFormat`/`VideoType` demotion is not modeled).
        let owns_alternates = |item: &BaseItemEntity| {
            group
                .values()
                .any(|other| other.primary_version_id.as_deref() == Some(item.id.as_str()))
        };
        let primary = items
            .iter()
            .find(|i| i.primary_version_id.is_none() && owns_alternates(i))
            .unwrap_or_else(|| {
                let mut best = &items[0];
                for item in &items[1..] {
                    if item.width.unwrap_or(0) > best.width.unwrap_or(0) {
                        best = item;
                    }
                }
                best
            });
        let primary_id = primary.id.clone();

        // Targeted single-column writes: the group rows were loaded to decide
        // the linkage, and a full-row save would write every other column back
        // stale — a scan or metadata refresh running concurrently (the bulk
        // tasks share the nightly window with RefreshLibrary) would have its
        // writes for these rows silently reverted.
        let Ok(primary_uuid) = Uuid::parse_str(&primary_id) else {
            return Ok(());
        };
        for item in group.values() {
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
        tracing::info!(
            merged = group.len() - 1,
            primary = %primary_id,
            "merge versions: merged alternate versions into primary"
        );
        Ok(())
    }

    /// Splits every item in `ids` from its version group, tolerating items
    /// that vanished mid-scan (the C# null-checks `GetItemById`).
    async fn split_each(
        &self,
        ids: &[Uuid],
        progress: Option<MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        for (index, &id) in ids.iter().enumerate() {
            report(progress, percent(index, ids.len()));
            match self.library.remove_alternate_sources(id).await {
                Ok(()) | Err(ServiceError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        report(progress, 100.0);
        Ok(())
    }
}

#[async_trait]
impl MergeVersionsManager for MergeVersionsService {
    async fn merge_movies(&self, progress: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        tracing::info!("merge versions: scanning for repeated movies");
        let movies = self.eligible_items(BaseItemKind::Movie).await?;
        let tmdb: HashMap<Uuid, String> = self
            .items
            .get_items_with_provider_id("Tmdb")
            .await?
            .into_iter()
            .collect();

        // Group the Tmdb-carrying movies by that id, tracking whether each
        // group still has a member that is not already an alternate (the
        // upstream duplicate filter).
        let mut groups: HashMap<String, (Vec<Uuid>, bool)> = HashMap::new();
        for movie in &movies {
            let Ok(id) = Uuid::parse_str(&movie.id) else {
                continue;
            };
            let Some(value) = tmdb.get(&id) else {
                continue; // no Tmdb id → skipped (`ProviderIds.ContainsKey`)
            };
            let entry = groups.entry(value.clone()).or_default();
            entry.0.push(id);
            entry.1 |= movie.primary_version_id.is_none();
        }
        let duplicates: Vec<Vec<Uuid>> = groups
            .into_values()
            .filter(|(ids, any_unmerged)| ids.len() > 1 && *any_unmerged)
            .map(|(ids, _)| ids)
            .collect();

        for (index, ids) in duplicates.iter().enumerate() {
            report(progress, percent(index, duplicates.len()));
            self.merge_group(ids).await?;
        }
        report(progress, 100.0);
        Ok(())
    }

    async fn split_movies(&self, progress: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        tracing::info!("merge versions: splitting all movies");
        // Upstream `SplitMovies` iterates `GetMoviesFromLibrary`, which keeps
        // only Tmdb-carrying eligible movies — the same set the merge scans.
        let movies = self.eligible_items(BaseItemKind::Movie).await?;
        let tmdb: HashMap<Uuid, String> = self
            .items
            .get_items_with_provider_id("Tmdb")
            .await?
            .into_iter()
            .collect();
        let ids: Vec<Uuid> = movies
            .iter()
            .filter_map(|m| Uuid::parse_str(&m.id).ok())
            .filter(|id| tmdb.contains_key(id))
            .collect();
        self.split_each(&ids, progress).await
    }

    async fn merge_episodes(
        &self,
        progress: Option<MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        tracing::info!("merge versions: scanning for repeated episodes");
        let episodes = self.eligible_items(BaseItemKind::Episode).await?;

        // The provider-id maps the merge key consults, in precedence order.
        let mut provider_maps: Vec<(&str, HashMap<Uuid, String>)> = Vec::new();
        for provider in ["Tvdb", "Tmdb", "Imdb"] {
            provider_maps.push((
                provider,
                self.items
                    .get_items_with_provider_id(provider)
                    .await?
                    .into_iter()
                    .collect(),
            ));
        }

        // The (case-insensitive) merge key per episode — the upstream 12.0 key
        // scoped to the series row (see `episode_merge_key`).
        let mut key_of: HashMap<Uuid, String> = HashMap::new();
        for ep in &episodes {
            let Ok(id) = Uuid::parse_str(&ep.id) else {
                continue;
            };
            let key = episode_merge_key(ep, |provider| {
                provider_maps
                    .iter()
                    .find(|(name, _)| *name == provider)
                    .and_then(|(_, map)| map.get(&id).cloned())
            });
            key_of.insert(id, key.to_lowercase());
        }

        // Self-heal before grouping: unlink any alternate whose key no longer
        // matches its primary's (e.g. groups created by the old name-scoped
        // key, which merged episodes across the series rows of two libraries).
        // Without this pass those links never converge — `expand_group`
        // re-accretes them into every new group, and a fully-linked stale
        // group has nothing left to merge, so it is never revisited.
        let mut healed = 0usize;
        for ep in &episodes {
            let (Ok(id), Some(pid)) = (
                Uuid::parse_str(&ep.id),
                ep.primary_version_id
                    .as_deref()
                    .and_then(|p| Uuid::parse_str(p).ok()),
            ) else {
                continue;
            };
            if let (Some(key), Some(primary_key)) = (key_of.get(&id), key_of.get(&pid))
                && key != primary_key
            {
                self.persistence.set_primary_version_id(id, None).await?;
                healed += 1;
            }
        }
        if healed > 0 {
            tracing::info!(
                healed,
                "merge versions: unlinked episode versions whose merge key no longer matches"
            );
        }

        let mut groups: HashMap<String, Vec<Uuid>> = HashMap::new();
        for ep in &episodes {
            if let Ok(id) = Uuid::parse_str(&ep.id)
                && let Some(key) = key_of.get(&id)
            {
                groups.entry(key.clone()).or_default().push(id);
            }
        }
        let duplicates: Vec<Vec<Uuid>> = groups.into_values().filter(|ids| ids.len() > 1).collect();
        tracing::info!(
            episodes = episodes.len(),
            duplicate_groups = duplicates.len(),
            "merge versions: episode scan"
        );

        for (index, ids) in duplicates.iter().enumerate() {
            report(progress, percent(index, duplicates.len()));
            self.merge_group(ids).await?;
        }
        report(progress, 100.0);
        Ok(())
    }

    async fn split_episodes(
        &self,
        progress: Option<MergeProgress<'_>>,
    ) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        tracing::info!("merge versions: splitting all episodes");
        let episodes = self.eligible_items(BaseItemKind::Episode).await?;
        let ids: Vec<Uuid> = episodes
            .iter()
            .filter_map(|e| Uuid::parse_str(&e.id).ok())
            .collect();
        self.split_each(&ids, progress).await
    }
}

/// Reports `pct` to the progress sink, if any.
fn report(progress: Option<MergeProgress<'_>>, pct: f64) {
    if let Some(p) = progress {
        p(pct);
    }
}

/// The upstream per-item progress percentage (`current / count * 100`).
#[allow(clippy::cast_precision_loss)]
fn percent(index: usize, total: usize) -> f64 {
    100.0 * (index + 1) as f64 / total.max(1) as f64
}

/// The upstream 12.0 `GetEpisodeMergeKey`: the first non-blank provider id
/// (`Tvdb` → `Tmdb` → `Imdb`), else season/episode numbers, else title
/// fields. The caller lowercases the key (`StringComparer.OrdinalIgnoreCase`).
fn episode_merge_key(ep: &BaseItemEntity, provider_id: impl Fn(&str) -> Option<String>) -> String {
    // Every branch is scoped to the series *identity* (presentation key), not
    // the series name — a deliberate divergence from upstream, which groups by
    // name. Two libraries holding the same show produce two series rows with
    // the same name, and a name-scoped key merged episodes across them: the
    // group's primary can live in only one of the two series, so the other
    // series showed missing episodes and undercounted seasons, while players
    // offered every library's copy as a "version". Scoping to the series row
    // keeps merging within one library's series, where a version list of the
    // same series' releases is what the user expects.
    let series = ep
        .series_presentation_unique_key
        .as_deref()
        .or(ep.series_id.as_deref())
        .or(ep.series_name.as_deref())
        .unwrap_or_default();
    for provider in ["Tvdb", "Tmdb", "Imdb"] {
        if let Some(value) = provider_id(provider)
            && !value.trim().is_empty()
        {
            return format!("{series}|provider:{provider}:{value}");
        }
    }
    if let (Some(parent), Some(index)) = (ep.parent_index_number, ep.index_number) {
        // No `IndexNumberEnd` column exists in Ferrofin's schema; the trailing
        // component is empty, exactly as the C# renders a null.
        return format!("{series}|number:{parent}:{index}:");
    }
    format!(
        "{series}|title:{}:{}:{}",
        ep.season_name.as_deref().unwrap_or_default(),
        ep.name.as_deref().unwrap_or_default(),
        ep.production_year
            .map(|y| y.to_string())
            .unwrap_or_default()
    )
}

/// Whether `path` sits under any of the excluded locations — the upstream
/// `IsInExcludedLibrary` (`LocationsExcluded.Any(s => ContainsSubPath(s, path))`).
fn in_excluded_location(excluded: &[String], path: Option<&str>) -> bool {
    path.is_some_and(|p| excluded.iter().any(|loc| contains_sub_path(loc, p)))
}

/// Whether an item's parent folder lies outside every virtual-folder location —
/// the upstream `IsInInactiveLibrary` (movies only). An item with no path (or
/// no parent) is treated as active, matching the C# null checks.
fn in_inactive_library(folders: &[VirtualFolderInfo], path: Option<&str>) -> bool {
    let Some(parent) = path
        .map(Path::new)
        .and_then(Path::parent)
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
    else {
        return false;
    };
    !folders
        .iter()
        .flat_map(|folder| &folder.locations)
        .any(|loc| loc.eq_ignore_ascii_case(&parent) || contains_sub_path(loc, &parent))
}

/// C# `IFileSystem.ContainsSubPath`: `path` lies strictly inside `parent`
/// (case-insensitive, separator-boundary-aware — `/a/b` contains `/a/b/c` but
/// not `/a/bc`).
fn contains_sub_path(parent: &str, path: &str) -> bool {
    let parent = parent.trim_end_matches(['/', '\\']);
    if parent.is_empty() {
        return false;
    }
    let parent = parent.to_lowercase();
    let path = path.to_lowercase();
    path.len() > parent.len()
        && path.starts_with(parent.as_str())
        && matches!(path.as_bytes()[parent.len()], b'/' | b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_core::{
        FerrofinItemCountService, FerrofinItemPersistenceService, FerrofinItemRepository,
        FerrofinLibraryManager, FerrofinPeopleRepository,
    };
    use ferrofin_db::Database;
    use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
    use ferrofin_traits::plugins::PluginImage;

    // ---- fakes -------------------------------------------------------------

    /// A plugin manager whose Merge Versions plugin has the given enabled flag
    /// and configuration.
    struct FakePlugins {
        enabled: bool,
        config: Vec<u8>,
    }

    impl FakePlugins {
        fn enabled_with(config: &str) -> Arc<dyn PluginManager> {
            Arc::new(Self {
                enabled: true,
                config: config.as_bytes().to_vec(),
            })
        }
        fn enabled() -> Arc<dyn PluginManager> {
            Self::enabled_with("{}")
        }
        fn disabled() -> Arc<dyn PluginManager> {
            Arc::new(Self {
                enabled: false,
                config: b"{}".to_vec(),
            })
        }
    }

    #[async_trait]
    impl PluginManager for FakePlugins {
        async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
            Ok(Some(PluginDescriptor {
                id,
                enabled: self.enabled,
                ..PluginDescriptor::default()
            }))
        }
        async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_plugin_configuration(&self, _id: Uuid) -> Result<Vec<u8>, ServiceError> {
            Ok(self.config.clone())
        }
        async fn set_plugin_configuration(
            &self,
            _id: Uuid,
            _config: Vec<u8>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
            Ok(None)
        }
        async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn set_repositories(
            &self,
            _repositories: Vec<RepositoryInfo>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
            Ok(Vec::new())
        }
    }

    /// A virtual-folder manager exposing a fixed set of library locations.
    struct FakeVirtualFolders(Vec<String>);

    #[async_trait]
    impl VirtualFolderManager for FakeVirtualFolders {
        async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
            Ok(vec![VirtualFolderInfo {
                locations: self.0.clone(),
                ..VirtualFolderInfo::default()
            }])
        }
        async fn add_virtual_folder(
            &self,
            _name: &str,
            _collection_type: Option<ferrofin_model::entities::CollectionTypeOptions>,
            _options: &ferrofin_model::configuration::LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn rename_virtual_folder(
            &self,
            _name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn add_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &ferrofin_model::configuration::MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn update_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &ferrofin_model::configuration::MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn remove_media_path(
            &self,
            _virtual_folder_name: &str,
            _path: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn update_library_options(
            &self,
            _virtual_folder_name: &str,
            _options: &ferrofin_model::configuration::LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
    }

    // ---- harness -----------------------------------------------------------

    async fn test_db() -> Database {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrations");
        db
    }

    fn service_over(
        db: &Database,
        plugins: Arc<dyn PluginManager>,
        locations: Vec<String>,
    ) -> MergeVersionsService {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let items = Arc::new(FerrofinItemRepository::new(db.clone(), lookup));
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let library = Arc::new(FerrofinLibraryManager::new(
            items.clone(),
            Arc::new(FerrofinItemCountService::new(db.clone())),
            persistence.clone(),
            Arc::new(FerrofinPeopleRepository::new(db.clone())),
        ));
        MergeVersionsService::new(
            items,
            persistence,
            library,
            Arc::new(FakeVirtualFolders(locations)),
            plugins,
        )
    }

    fn stored_type(kind: BaseItemKind) -> &'static str {
        ferrofin_core::item_type_lookup::stored_type_name(kind).expect("stored type name")
    }

    /// Persists a minimal item row of the given kind (via the real persistence
    /// service — no raw SQL outside the repository boundary).
    async fn seed(db: &Database, id: Uuid, kind: BaseItemKind, path: Option<&str>, width: i64) {
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[BaseItemEntity {
                id: id.to_string(),
                type_: stored_type(kind).to_owned(),
                path: path.map(str::to_owned),
                width: Some(width),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("seed item");
    }

    /// Attaches a `(ProviderId, ProviderValue)` external id to an item.
    async fn set_provider_id(db: &Database, id: Uuid, key: &str, value: &str) {
        FerrofinItemPersistenceService::new(db.clone())
            .save_provider_id(id, key, value)
            .await
            .expect("set provider id");
    }

    /// Sets the episode-grouping columns the merge key reads (read-modify-write
    /// through the repository + persistence seams).
    async fn set_episode_fields(
        db: &Database,
        id: Uuid,
        series: &str,
        season: &str,
        name: &str,
        numbers: Option<(i64, i64)>,
        year: i64,
    ) {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let mut item = FerrofinItemRepository::new(db.clone(), lookup)
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row");
        item.series_name = Some(series.to_owned());
        item.season_name = Some(season.to_owned());
        item.name = Some(name.to_owned());
        item.parent_index_number = numbers.map(|(p, _)| p);
        item.index_number = numbers.map(|(_, i)| i);
        item.production_year = Some(year);
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[item])
            .await
            .expect("set episode fields");
    }

    async fn primary_of(db: &Database, service: &MergeVersionsService, id: Uuid) -> Option<String> {
        let _ = db;
        service
            .items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row")
            .primary_version_id
    }

    // ---- extension surface -------------------------------------------------

    #[test]
    fn extension_default_config_round_trips() {
        let bytes = MergeVersionsExtension.default_config();
        let config: MergeVersionsConfig = serde_json::from_slice(&bytes).expect("valid JSON");
        assert!(config.locations_excluded.is_empty());
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert!(
            v["LocationsExcluded"].as_array().expect("array").is_empty(),
            "config must serialize PascalCase for the settings page"
        );
    }

    #[test]
    fn descriptor_uses_the_upstream_guid() {
        let d = MergeVersionsExtension.descriptor();
        assert_eq!(
            d.id.to_string(),
            "f21bbed8-3a97-4d8b-88b2-48aaa65427cb",
            "the settings page hardcodes this GUID"
        );
        assert_eq!(d.name, "Merge Versions");
    }

    #[test]
    fn config_page_is_the_vendored_upstream_page() {
        let pages = MergeVersionsExtension.config_pages();
        assert_eq!(pages.len(), 1);
        let html = String::from_utf8(pages[0].bytes.clone()).expect("utf8");
        assert!(html.contains("f21bbed8-3a97-4d8b-88b2-48aaa65427cb"));
        assert!(html.contains("MergeVersions/MergeMovies"));
    }

    // ---- pure helpers ------------------------------------------------------

    #[test]
    fn contains_sub_path_is_boundary_aware_and_case_insensitive() {
        assert!(contains_sub_path("/a/b", "/a/b/c.mkv"));
        assert!(contains_sub_path("/a/b/", "/a/b/c.mkv"));
        assert!(contains_sub_path("/A/B", "/a/b/c.mkv"));
        assert!(!contains_sub_path("/a/b", "/a/bc/c.mkv"));
        assert!(!contains_sub_path("/a/b", "/a/b"));
        assert!(!contains_sub_path("", "/a/b"));
    }

    fn episode(
        series: &str,
        season: &str,
        name: &str,
        numbers: Option<(i64, i64)>,
    ) -> BaseItemEntity {
        BaseItemEntity {
            series_name: Some(series.to_owned()),
            season_name: Some(season.to_owned()),
            name: Some(name.to_owned()),
            parent_index_number: numbers.map(|(p, _)| p),
            index_number: numbers.map(|(_, i)| i),
            production_year: Some(2020),
            ..BaseItemEntity::default()
        }
    }

    #[test]
    fn episode_merge_key_prefers_provider_ids_in_order() {
        let ep = episode("Show", "Season 1", "Pilot", Some((1, 1)));
        // Tvdb outranks Tmdb outranks Imdb.
        let key = episode_merge_key(&ep, |p| match p {
            "Tvdb" => Some("111".to_owned()),
            "Tmdb" => Some("222".to_owned()),
            _ => None,
        });
        assert_eq!(key, "Show|provider:Tvdb:111");
        let key = episode_merge_key(&ep, |p| (p == "Imdb").then(|| "tt1".to_owned()));
        assert_eq!(key, "Show|provider:Imdb:tt1");
        // A blank provider id falls through to the next source.
        let key = episode_merge_key(&ep, |p| match p {
            "Tvdb" => Some("  ".to_owned()),
            "Tmdb" => Some("222".to_owned()),
            _ => None,
        });
        assert_eq!(key, "Show|provider:Tmdb:222");
    }

    #[test]
    fn episode_merge_key_falls_back_to_numbers_then_title() {
        let ep = episode("Show", "Season 1", "Pilot", Some((1, 2)));
        assert_eq!(episode_merge_key(&ep, |_| None), "Show|number:1:2:");
        let ep = episode("Show", "Season 1", "Pilot", None);
        assert_eq!(
            episode_merge_key(&ep, |_| None),
            "Show|title:Season 1:Pilot:2020"
        );
    }

    // Same show name + numbers in two different series rows (a show present in
    // two libraries) must produce different keys — the series identity, not
    // its name, scopes the group. The series presentation key outranks the
    // name in every branch, including the provider one.
    #[test]
    fn episode_merge_key_scopes_to_the_series_row() {
        let mut hot = episode("Show", "Season 1", "Pilot", Some((1, 1)));
        hot.series_presentation_unique_key = Some("hotkey".to_owned());
        let mut cold = episode("Show", "Season 1", "Pilot", Some((1, 1)));
        cold.series_presentation_unique_key = Some("coldkey".to_owned());
        assert_ne!(
            episode_merge_key(&hot, |_| None),
            episode_merge_key(&cold, |_| None)
        );
        // Even a shared Tvdb episode id must not merge across series rows.
        let tvdb = |p: &str| (p == "Tvdb").then(|| "999".to_owned());
        assert_ne!(
            episode_merge_key(&hot, tvdb),
            episode_merge_key(&cold, tvdb)
        );
        assert_eq!(episode_merge_key(&hot, tvdb), "hotkey|provider:Tvdb:999");
    }

    #[test]
    fn inactive_library_checks_the_parent_folder() {
        let folders = vec![VirtualFolderInfo {
            locations: vec!["/media/movies".to_owned()],
            ..VirtualFolderInfo::default()
        }];
        // Parent under (or equal to) a library location → active.
        assert!(!in_inactive_library(
            &folders,
            Some("/media/movies/Film/f.mkv")
        ));
        assert!(!in_inactive_library(&folders, Some("/media/movies/f.mkv")));
        // Parent outside every location → inactive.
        assert!(in_inactive_library(&folders, Some("/other/Film/f.mkv")));
        // No path → treated as active (the C# null check).
        assert!(!in_inactive_library(&folders, None));
    }

    // ---- service over a real database --------------------------------------

    #[tokio::test]
    async fn merge_movies_groups_by_tmdb() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x401);
        let b = Uuid::from_u128(0x402);
        let lonely = Uuid::from_u128(0x403);
        let no_id = Uuid::from_u128(0x404);
        seed(&db, a, BaseItemKind::Movie, None, 1920).await;
        seed(&db, b, BaseItemKind::Movie, None, 640).await;
        seed(&db, lonely, BaseItemKind::Movie, None, 640).await;
        seed(&db, no_id, BaseItemKind::Movie, None, 640).await;
        set_provider_id(&db, a, "Tmdb", "603").await;
        set_provider_id(&db, b, "Tmdb", "603").await;
        set_provider_id(&db, lonely, "Tmdb", "604").await;
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());

        svc.merge_movies(None).await.expect("merge movies");

        // a (widest) is the primary; b links to it; the rest are untouched.
        assert_eq!(primary_of(&db, &svc, a).await, None);
        assert_eq!(
            primary_of(&db, &svc, b).await.as_deref(),
            Some(a.to_string().as_str())
        );
        assert_eq!(primary_of(&db, &svc, lonely).await, None);
        assert_eq!(primary_of(&db, &svc, no_id).await, None);
    }

    #[tokio::test]
    async fn merge_movies_keeps_an_existing_primary() {
        let db = test_db().await;
        let primary = Uuid::from_u128(0x411);
        let alt = Uuid::from_u128(0x412);
        let wider = Uuid::from_u128(0x413);
        seed(&db, primary, BaseItemKind::Movie, None, 640).await;
        seed(&db, alt, BaseItemKind::Movie, None, 480).await;
        seed(&db, wider, BaseItemKind::Movie, None, 3840).await;
        for id in [primary, alt, wider] {
            set_provider_id(&db, id, "Tmdb", "603").await;
        }
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());
        // Pre-merge primary+alt: primary owns an alternate.
        svc.merge_group(&[primary, alt]).await.expect("pre-merge");
        assert_eq!(
            primary_of(&db, &svc, alt).await.as_deref(),
            Some(primary.to_string().as_str())
        );

        svc.merge_movies(None).await.expect("merge movies");

        // The 12.0 primary probe keeps the established primary even though a
        // wider newcomer joined the group.
        assert_eq!(primary_of(&db, &svc, primary).await, None);
        for id in [alt, wider] {
            assert_eq!(
                primary_of(&db, &svc, id).await.as_deref(),
                Some(primary.to_string().as_str()),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn merge_group_regroups_nested_families() {
        let db = test_db().await;
        // Two established pairs: (a1←a2) and (b1←b2). Merging a2+b2 must
        // re-group the whole family under one primary, not nest pointers.
        let a1 = Uuid::from_u128(0x421);
        let a2 = Uuid::from_u128(0x422);
        let b1 = Uuid::from_u128(0x423);
        let b2 = Uuid::from_u128(0x424);
        for (id, w) in [(a1, 1920), (a2, 640), (b1, 1280), (b2, 480)] {
            seed(&db, id, BaseItemKind::Movie, None, w).await;
        }
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());
        svc.merge_group(&[a1, a2]).await.expect("group a");
        svc.merge_group(&[b1, b2]).await.expect("group b");

        svc.merge_group(&[a2, b2]).await.expect("regroup");

        // One primary; every other member points straight at it.
        let mut primaries = Vec::new();
        for id in [a1, a2, b1, b2] {
            if primary_of(&db, &svc, id).await.is_none() {
                primaries.push(id);
            }
        }
        assert_eq!(primaries.len(), 1, "exactly one primary after regroup");
        let top = primaries[0].to_string();
        for id in [a1, a2, b1, b2] {
            if id != primaries[0] {
                assert_eq!(
                    primary_of(&db, &svc, id).await.as_deref(),
                    Some(top.as_str()),
                    "{id} must point at the single primary"
                );
            }
        }
    }

    #[tokio::test]
    async fn split_movies_clears_every_group() {
        let db = test_db().await;
        let primary = Uuid::from_u128(0x431);
        let alt = Uuid::from_u128(0x432);
        seed(&db, primary, BaseItemKind::Movie, None, 1920).await;
        seed(&db, alt, BaseItemKind::Movie, None, 640).await;
        for id in [primary, alt] {
            set_provider_id(&db, id, "Tmdb", "603").await;
        }
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());
        svc.merge_movies(None).await.expect("merge");
        assert!(primary_of(&db, &svc, alt).await.is_some());

        svc.split_movies(None).await.expect("split");

        for id in [primary, alt] {
            assert_eq!(primary_of(&db, &svc, id).await, None);
        }
    }

    #[tokio::test]
    async fn merge_episodes_prefers_provider_ids_over_titles() {
        let db = test_db().await;
        // Same Tvdb id but different titles → still one group. A third
        // episode with a different Tvdb id stays out even with equal titles.
        let a = Uuid::from_u128(0x441);
        let b = Uuid::from_u128(0x442);
        let other = Uuid::from_u128(0x443);
        seed(&db, a, BaseItemKind::Episode, None, 1920).await;
        seed(&db, b, BaseItemKind::Episode, None, 640).await;
        seed(&db, other, BaseItemKind::Episode, None, 640).await;
        set_episode_fields(&db, a, "Show", "Season 1", "Pilot", Some((1, 1)), 2020).await;
        set_episode_fields(
            &db,
            b,
            "Show",
            "Season 1",
            "Pilot (Extended)",
            Some((1, 1)),
            2020,
        )
        .await;
        set_episode_fields(&db, other, "Show", "Season 1", "Pilot", Some((1, 1)), 2020).await;
        set_provider_id(&db, a, "Tvdb", "555").await;
        set_provider_id(&db, b, "Tvdb", "555").await;
        set_provider_id(&db, other, "Tvdb", "556").await;
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());

        svc.merge_episodes(None).await.expect("merge episodes");

        assert_eq!(
            primary_of(&db, &svc, b).await.as_deref(),
            Some(a.to_string().as_str())
        );
        assert_eq!(primary_of(&db, &svc, other).await, None);
    }

    #[tokio::test]
    async fn merge_episodes_groups_by_numbers_without_provider_ids() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x451);
        let b = Uuid::from_u128(0x452);
        let other = Uuid::from_u128(0x453);
        for id in [a, b, other] {
            seed(&db, id, BaseItemKind::Episode, None, 640).await;
        }
        set_episode_fields(&db, a, "Show", "Season 1", "Pilot", Some((1, 1)), 2020).await;
        set_episode_fields(&db, b, "SHOW", "Season 1", "Pilot", Some((1, 1)), 2020).await;
        set_episode_fields(&db, other, "Show", "Season 1", "Pilot", Some((1, 2)), 2020).await;
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());

        svc.merge_episodes(None).await.expect("merge episodes");

        // a+b share `show|number:1:1:` (case-insensitive); `other` differs.
        assert!(
            primary_of(&db, &svc, a).await.is_some() != primary_of(&db, &svc, b).await.is_some(),
            "one of a/b is the primary, the other the alternate"
        );
        assert_eq!(primary_of(&db, &svc, other).await, None);
    }

    /// Sets the series-row identity the scoped merge key reads.
    async fn set_series_key(db: &Database, id: Uuid, key: &str) {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let mut item = FerrofinItemRepository::new(db.clone(), lookup)
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row");
        item.series_presentation_unique_key = Some(key.to_owned());
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[item])
            .await
            .expect("save series key");
    }

    // The merge key is scoped to the series ROW: the same show in two
    // libraries (two series rows, one name) must not merge across them, and
    // the bulk task must unlink legacy cross-series links (which the old
    // name-scoped key created) so each library's series regroups internally.
    #[tokio::test]
    async fn merge_episodes_scopes_to_series_row_and_heals_cross_series_links() {
        let db = test_db().await;
        let (hot1, hot2) = (Uuid::from_u128(0x471), Uuid::from_u128(0x472));
        let (cold1, cold2) = (Uuid::from_u128(0x473), Uuid::from_u128(0x474));
        for (id, width) in [(hot1, 1920), (hot2, 640), (cold1, 1920), (cold2, 640)] {
            seed(&db, id, BaseItemKind::Episode, None, width).await;
            set_episode_fields(&db, id, "Show", "Season 1", "Pilot", Some((1, 1)), 2020).await;
        }
        for id in [hot1, hot2] {
            set_series_key(&db, id, "aaaahotseries").await;
        }
        for id in [cold1, cold2] {
            set_series_key(&db, id, "bbbbcoldseries").await;
        }
        // The legacy name-scoped key merged all four across both series.
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        for id in [hot2, cold1, cold2] {
            persistence
                .set_primary_version_id(id, Some(hot1))
                .await
                .expect("seed stale link");
        }

        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());
        svc.merge_episodes(None).await.expect("merge episodes");

        // Hot series: hot2 stays (or is re-linked) under hot1.
        assert_eq!(
            primary_of(&db, &svc, hot2).await,
            Some(hot1.to_string()),
            "same-series link survives"
        );
        assert_eq!(primary_of(&db, &svc, hot1).await, None);
        // Cold series: unlinked from hot1 and regrouped among themselves —
        // the widest (cold1) is primary, cold2 its alternate.
        assert_eq!(
            primary_of(&db, &svc, cold2).await,
            Some(cold1.to_string()),
            "cold pair regroups within its own series"
        );
        assert_eq!(primary_of(&db, &svc, cold1).await, None);
    }

    #[tokio::test]
    async fn excluded_locations_skip_matching_items() {
        let db = test_db().await;
        let inside = Uuid::from_u128(0x461);
        let excluded_a = Uuid::from_u128(0x462);
        let excluded_b = Uuid::from_u128(0x463);
        seed(
            &db,
            inside,
            BaseItemKind::Movie,
            Some("/media/movies/Film/f1.mkv"),
            1920,
        )
        .await;
        seed(
            &db,
            excluded_a,
            BaseItemKind::Movie,
            Some("/media/kids/Film/f2.mkv"),
            640,
        )
        .await;
        seed(
            &db,
            excluded_b,
            BaseItemKind::Movie,
            Some("/media/kids/Film/f3.mkv"),
            480,
        )
        .await;
        for id in [inside, excluded_a, excluded_b] {
            set_provider_id(&db, id, "Tmdb", "603").await;
        }
        let plugins = FakePlugins::enabled_with(r#"{"LocationsExcluded":["/media/kids"]}"#);
        let svc = service_over(
            &db,
            plugins,
            vec!["/media/movies".to_owned(), "/media/kids".to_owned()],
        );

        svc.merge_movies(None).await.expect("merge movies");

        // The two duplicates inside the excluded location were never merged
        // (with them filtered out, `inside` has no duplicate left).
        for id in [inside, excluded_a, excluded_b] {
            assert_eq!(primary_of(&db, &svc, id).await, None, "{id}");
        }
    }

    #[tokio::test]
    async fn inactive_library_movies_are_skipped() {
        let db = test_db().await;
        let active = Uuid::from_u128(0x471);
        let inactive = Uuid::from_u128(0x472);
        seed(
            &db,
            active,
            BaseItemKind::Movie,
            Some("/media/movies/Film/f1.mkv"),
            1920,
        )
        .await;
        // Same movie, but its folder lies outside every virtual folder.
        seed(
            &db,
            inactive,
            BaseItemKind::Movie,
            Some("/detached/Film/f2.mkv"),
            640,
        )
        .await;
        for id in [active, inactive] {
            set_provider_id(&db, id, "Tmdb", "603").await;
        }
        let svc = service_over(
            &db,
            FakePlugins::enabled(),
            vec!["/media/movies".to_owned()],
        );

        svc.merge_movies(None).await.expect("merge movies");

        // With the inactive copy filtered out there is no duplicate to merge.
        for id in [active, inactive] {
            assert_eq!(primary_of(&db, &svc, id).await, None, "{id}");
        }
    }

    #[tokio::test]
    async fn disabled_plugin_rejects_the_bulk_ops() {
        let db = test_db().await;
        let svc = service_over(&db, FakePlugins::disabled(), Vec::new());
        for result in [
            svc.merge_movies(None).await,
            svc.split_movies(None).await,
            svc.merge_episodes(None).await,
            svc.split_episodes(None).await,
        ] {
            assert!(matches!(result, Err(ServiceError::NotFound(_))));
        }
    }

    #[tokio::test]
    async fn progress_reports_and_reaches_completion() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x481);
        let b = Uuid::from_u128(0x482);
        seed(&db, a, BaseItemKind::Movie, None, 1920).await;
        seed(&db, b, BaseItemKind::Movie, None, 640).await;
        for id in [a, b] {
            set_provider_id(&db, id, "Tmdb", "603").await;
        }
        let svc = service_over(&db, FakePlugins::enabled(), Vec::new());

        let reported = std::sync::Mutex::new(Vec::new());
        let sink = |p: f64| reported.lock().unwrap().push(p);
        svc.merge_movies(Some(&sink)).await.expect("merge");

        let reported = reported.into_inner().unwrap();
        assert!(!reported.is_empty());
        assert_eq!(reported.last().copied(), Some(100.0));
    }

    #[tokio::test]
    async fn tasks_register_the_upstream_pair_and_gate_on_enabled() {
        let db = test_db().await;
        let svc = Arc::new(service_over(&db, FakePlugins::disabled(), Vec::new()));
        let plugins: Arc<dyn PluginManager> = Arc::new(FakePlugins {
            enabled: false,
            config: b"{}".to_vec(),
        });
        let cx = ExtensionContext {
            library: Arc::new(FerrofinLibraryManager::new(
                Arc::new(FerrofinItemRepository::new(
                    db.clone(),
                    Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new()),
                )),
                Arc::new(FerrofinItemCountService::new(db.clone())),
                Arc::new(FerrofinItemPersistenceService::new(db.clone())),
                Arc::new(FerrofinPeopleRepository::new(db.clone())),
            )),
            media_segments: Arc::new(ferrofin_core::FerrofinMediaSegmentManager::new(
                db.clone(),
                Arc::new(FerrofinLibraryManager::new(
                    Arc::new(FerrofinItemRepository::new(
                        db.clone(),
                        Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new()),
                    )),
                    Arc::new(FerrofinItemCountService::new(db.clone())),
                    Arc::new(FerrofinItemPersistenceService::new(db.clone())),
                    Arc::new(FerrofinPeopleRepository::new(db.clone())),
                )),
            )),
            plugins,
            fingerprinter: None,
            cache_dir: std::env::temp_dir(),
            merge_versions: svc,
        };

        let tasks = MergeVersionsExtension.tasks(&cx);
        let keys: Vec<&str> = tasks.iter().map(|t| t.key()).collect();
        assert_eq!(keys, ["MergeMoviesTask", "MergeEpisodesTask"]);
        for task in &tasks {
            assert_eq!(task.category(), "Merge Versions");
            let triggers = task.default_triggers();
            assert_eq!(triggers[0].interval_ticks, Some(DAY_TICKS));
            // Startup trigger: the interval clock resets each boot, so without
            // this a frequently-restarted server never merges at all.
            assert_eq!(triggers[1].type_, TaskTriggerInfoType::StartupTrigger);
            // Disabled plugin → the task is a silent no-op, not a failure.
            let progress = TaskProgress::default();
            task.execute(&progress).await.expect("skip when disabled");
        }
    }
}
