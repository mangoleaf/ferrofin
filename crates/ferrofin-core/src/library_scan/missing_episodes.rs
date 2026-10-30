//! Port of `TmdbMissingEpisodeProvider` (Jellyfin v12.2,
//! `MediaBrowser.Providers/Plugins/Tmdb/TV/TmdbMissingEpisodeProvider.cs`),
//! the series' custom provider that keeps its virtual (path-less) episodes in
//! step with TheMovieDb while the TMDb settings page's unaired and/or missing
//! episode options are on and the series' library is opted in: it creates
//! one for each episode TMDb lists that the library lacks (and a virtual
//! season for a season it has nothing of), keeps them in sync with TMDb, and
//! removes them once aged out, unwanted, superseded by a file, or turned off.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Datelike as _, NaiveDate, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_providers::tmdb::{EpisodeDetails, MissingEpisodeSettings, SeasonDetails};
use ferrofin_traits::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::providers::MetadataRefreshOptions;
use ferrofin_util::directory_path::DirectoryPath;
use uuid::Uuid;

use super::{ArtworkCache, LibraryScanner, ScanRun, SeasonLookup, item_type_lookup, parse_ymd};

/// The provider name upstream's TMDb ids are stored under.
const TMDB: &str = "Tmdb";

/// A series the walk refreshed, with what the provider needs of its refresh:
/// its library (the opt-in is per library) and the TMDb language its
/// episodes' season lookups were made in, so the scan's cached season
/// responses are shared.
#[derive(Debug, Clone)]
pub(super) struct MissingEpisodeSeries {
    /// The series.
    pub(super) id: Uuid,
    /// Its collection folder.
    pub(super) library: Option<Uuid>,
    /// `GetPreferredMetadataLanguage` (with its country), as TMDb takes it.
    pub(super) language: Option<String>,
}

/// `TmdbMissingEpisodeProvider.ShouldImportEpisode`: an undated episode
/// never; a special only with `import_specials`; then one airing today or
/// later with `import_unaired`, one already aired with `import_missing`.
// Upstream's parameter list, which its oracle cases call it with.
#[allow(clippy::fn_params_excessive_bools)]
pub(super) fn should_import_episode(
    premiere: Option<DateTime<Utc>>,
    today: NaiveDate,
    import_unaired: bool,
    import_missing: bool,
    is_special: bool,
    import_specials: bool,
) -> bool {
    let Some(premiere) = premiere else {
        return false;
    };
    if is_special && !import_specials {
        return false;
    }
    if premiere.date_naive() >= today {
        import_unaired
    } else {
        import_missing
    }
}

/// What `ShouldPrune` reads of an episode.
#[derive(Debug, Clone, Copy)]
pub(super) struct PruneFacts {
    /// `IsVirtualItem`.
    pub(super) virtual_item: bool,
    /// It carries a TMDb id: this provider created it.
    pub(super) has_tmdb_id: bool,
    /// `ParentIndexNumber`.
    pub(super) season: Option<i64>,
    /// `PremiereDate`.
    pub(super) premiere: Option<DateTime<Utc>>,
}

/// `TmdbMissingEpisodeProvider.ShouldPrune`: one of this provider's virtual
/// episodes goes when it is a special and specials are unwanted, or — while
/// missing episodes are off (`prune_aged_out`) — once it aired more than
/// `grace_days` ago.
pub(super) fn should_prune(
    episode: PruneFacts,
    prune_aged_out: bool,
    today: NaiveDate,
    grace_days: i32,
    import_specials: bool,
) -> bool {
    if !episode.virtual_item || !episode.has_tmdb_id {
        return false;
    }
    if episode.season == Some(0) && !import_specials {
        return true;
    }
    let cutoff = today - chrono::Days::new(u64::try_from(grace_days).unwrap_or(0));
    prune_aged_out && episode.premiere.is_some_and(|p| p.date_naive() < cutoff)
}

/// `TmdbMissingEpisodeProvider.GetPremiereDate`: the air date as the TMDb
/// episode provider stores it.
///
/// TODO(parity, open work item — gl-490): upstream keeps a date-only value
/// at server-local midnight converted to UTC; Ferrofin stores every
/// provider date-only value at UTC midnight (`parse_ymd`), the owner
/// decision gl-490 settles for all of them at once.
pub(super) fn premiere_date(air_date: Option<&str>) -> Option<DateTime<Utc>> {
    air_date.and_then(parse_ymd)
}

/// `TmdbMissingEpisodeProvider.UpdateVirtualEpisode`: TMDb's non-empty name
/// and overview, and its air date (with its year), where they differ.
/// Returns whether anything changed.
pub(super) fn update_virtual_episode(
    row: &mut BaseItemEntity,
    tmdb: &EpisodeDetails,
    premiere: Option<DateTime<Utc>>,
) -> bool {
    let mut changed = false;
    if let Some(name) = tmdb.name.as_deref().filter(|n| !n.is_empty())
        && row.name.as_deref() != Some(name)
    {
        row.name = Some(name.to_owned());
        changed = true;
    }
    if let Some(overview) = tmdb.overview.as_deref().filter(|o| !o.is_empty())
        && row.overview.as_deref() != Some(overview)
    {
        row.overview = Some(overview.to_owned());
        changed = true;
    }
    if let Some(premiere) = premiere
        && row.premiere_date != Some(premiere)
    {
        row.premiere_date = Some(premiere);
        row.production_year = Some(i64::from(premiere.year()));
        changed = true;
    }
    changed
}

/// What `BuildSeasonSortNameTemplate` reads of a season.
#[derive(Debug, Clone, Copy)]
pub(super) struct SeasonFacts<'a> {
    /// `IsVirtualItem`.
    pub(super) virtual_item: bool,
    /// `IndexNumber`.
    pub(super) number: Option<i64>,
    /// `ForcedSortName`.
    pub(super) forced_sort_name: Option<&'a str>,
}

/// `TmdbMissingEpisodeProvider.BuildSeasonSortNameTemplate`: the forced sort
/// name a virtual season numbered `n` takes so it interleaves with physical
/// seasons that sort by name (a season NFO's `Season 01`) — the first such
/// physical season's text around its last run of digits, with `n` padded to
/// that run's width. [`None`] when no physical season sorts by name, or its
/// name has no digits.
///
/// Digits are ASCII ones (`char::is_ascii_digit`); upstream's
/// `char.IsDigit` also takes other scripts' decimal digits.
pub(super) fn season_sort_name_template(
    seasons: &[SeasonFacts<'_>],
) -> Option<impl Fn(i64) -> String + use<>> {
    let forced: Vec<char> = seasons
        .iter()
        .find(|s| {
            !s.virtual_item
                && s.number.is_some()
                && s.forced_sort_name.is_some_and(|n| !n.is_empty())
        })?
        .forced_sort_name?
        .chars()
        .collect();
    let end = forced.iter().rposition(char::is_ascii_digit)?;
    let start = forced[..=end]
        .iter()
        .rposition(|c| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let prefix: String = forced[..start].iter().collect();
    let suffix: String = forced[end + 1..].iter().collect();
    let width = end - start + 1;
    Some(move |number: i64| format!("{prefix}{number:0width$}{suffix}"))
}

/// One of the series' stored episodes, as the provider weighs it.
struct StoredEpisode {
    row: BaseItemEntity,
    tmdb_id: Option<String>,
}

impl StoredEpisode {
    fn id(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.row.id).ok()
    }

    fn key(&self) -> Option<(i64, i64)> {
        Some((self.row.parent_index_number?, self.row.index_number?))
    }

    /// Created by this provider: virtual, with a TMDb id.
    fn ours(&self) -> bool {
        self.row.is_virtual_item && self.tmdb_id.is_some()
    }

    fn facts(&self) -> PruneFacts {
        PruneFacts {
            virtual_item: self.row.is_virtual_item,
            has_tmdb_id: self.tmdb_id.is_some(),
            season: self.row.parent_index_number,
            premiere: self.row.premiere_date,
        }
    }
}

/// One of the series' seasons, by number.
#[derive(Clone)]
struct StoredSeason {
    id: Uuid,
    name: Option<String>,
    row: BaseItemEntity,
}

/// What one series' pass changed, for the closing `LibraryChanged`.
#[derive(Default)]
struct Changes {
    added: Vec<Uuid>,
    removed: Vec<Uuid>,
    updated: Vec<Uuid>,
}

/// A series' facts every row the provider creates copies.
struct SeriesContext<'a> {
    row: &'a BaseItemEntity,
    id: Uuid,
    key: String,
    ancestors: Vec<Uuid>,
}

impl LibraryScanner {
    /// `TmdbMissingEpisodeProvider.FetchAsync` for each of `series`, the
    /// series this scan refreshed, run in the closing passes once the walk
    /// saved every row (so a file the walk just added counts), and before
    /// `RemoveObsoleteEpisodes`, as upstream runs a custom provider before
    /// `AfterMetadataRefresh`. Season responses the walk fetched for the
    /// episodes are reused from `cache`.
    pub(super) async fn refresh_missing_episodes(
        &self,
        series: &[MissingEpisodeSeries],
        cache: &ArtworkCache,
        run: ScanRun<'_>,
    ) -> Result<(), ServiceError> {
        let (Some(tmdb), Some(items)) = (&self.tmdb, &self.item_repository) else {
            return Ok(());
        };
        if series.is_empty() {
            return Ok(());
        }
        let settings = tmdb.missing_episode_settings().await;
        // Nothing on and nothing created before: no series has anything to
        // prune, so none is read.
        if !settings.imports() && !self.has_tmdb_virtual_episodes(items.as_ref()).await? {
            return Ok(());
        }
        let today = Utc::now().date_naive();
        let mut changes = Changes::default();
        for one in series {
            if run.cancel.is_cancelled() {
                break;
            }
            // One series failing leaves the others to go on, as a failed
            // series refresh does upstream; the next refresh retries it.
            if let Err(err) = self
                .refresh_series_missing_episodes(
                    items.as_ref(),
                    one,
                    &settings,
                    cache,
                    today,
                    &mut changes,
                )
                .await
            {
                tracing::warn!(
                    item_id = %one.id,
                    %err,
                    "could not refresh the series' missing and unaired episodes; the next refresh retries"
                );
            }
        }
        if !changes.added.is_empty() || !changes.removed.is_empty() {
            tracing::info!(
                added = changes.added.len(),
                removed = changes.removed.len(),
                updated = changes.updated.len(),
                "refreshed TMDb missing and unaired episodes"
            );
        }
        self.announce_missing_episode_changes(items.as_ref(), &changes)
            .await;
        Ok(())
    }

    /// Whether any stored episode is one of this provider's (virtual, with a
    /// TMDb id) — one id-only probe.
    async fn has_tmdb_virtual_episodes(
        &self,
        items: &dyn ItemRepository,
    ) -> Result<bool, ServiceError> {
        Ok(!items
            .get_item_ids(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Episode],
                is_virtual_item: Some(true),
                has_tmdb_id: Some(true),
                limit: Some(1),
                ..Default::default()
            })
            .await?
            .is_empty())
    }

    /// One series' `FetchAsync`.
    // The provider's whole decision for one series, in upstream's order; a
    // helper per step would only re-thread the same state.
    #[allow(clippy::too_many_lines)]
    async fn refresh_series_missing_episodes(
        &self,
        items: &dyn ItemRepository,
        one: &MissingEpisodeSeries,
        settings: &MissingEpisodeSettings,
        cache: &ArtworkCache,
        today: NaiveDate,
        changes: &mut Changes,
    ) -> Result<(), ServiceError> {
        let Some(series_row) = items.retrieve_item(one.id).await? else {
            return Ok(());
        };
        let key = series_row
            .presentation_unique_key
            .clone()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| series_row.id.clone());
        let rows = items
            .get_item_list(&InternalItemsQuery {
                series_presentation_unique_key: Some(key.clone()),
                include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Season],
                ..Default::default()
            })
            .await?;
        let season_type =
            item_type_lookup::stored_type_name(BaseItemKind::Season).unwrap_or_default();
        let (season_rows, episode_rows): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|r| r.type_ == season_type);
        let mut ids: Vec<Uuid> = episode_rows
            .iter()
            .chain(&season_rows)
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        ids.push(one.id);
        let mut provider_ids = self.persistence.provider_ids_for_items(&ids).await?;
        let mut tmdb_of = |id: &str| -> Option<String> {
            Uuid::parse_str(id)
                .ok()
                .and_then(|id| provider_ids.remove(&id))?
                .into_iter()
                .find(|(provider, value)| provider.eq_ignore_ascii_case(TMDB) && !value.is_empty())
                .map(|(_, value)| value)
        };
        let series_tmdb = tmdb_of(&series_row.id);
        let episodes: Vec<StoredEpisode> = episode_rows
            .into_iter()
            .map(|row| {
                let tmdb_id = tmdb_of(&row.id);
                StoredEpisode { row, tmdb_id }
            })
            .collect();
        let season_tmdb: HashMap<String, Option<String>> = season_rows
            .iter()
            .map(|r| (r.id.clone(), tmdb_of(&r.id)))
            .collect();

        // Inactive here — both options off, or the library not opted in:
        // every virtual episode it created goes.
        let active = settings.imports() && one.library.is_some_and(|l| settings.library_enabled(l));
        if !active {
            let doomed: Vec<Uuid> = episodes
                .iter()
                .filter(|e| e.ours())
                .filter_map(StoredEpisode::id)
                .collect();
            for episode in episodes.iter().filter(|e| e.ours()) {
                tracing::debug!(
                    item_id = %episode.row.id,
                    series_name = series_row.name.as_deref().unwrap_or_default(),
                    season_number = episode.row.parent_index_number,
                    episode_number = episode.row.index_number,
                    reason = "the TMDb missing episode provider is disabled for this library",
                    "removing a virtual episode"
                );
            }
            self.remove_episodes(&doomed, changes).await?;
            self.remove_emptied_seasons(items, &season_rows, &season_tmdb, changes)
                .await?;
            return Ok(());
        }
        let Some(tmdb_id) = series_tmdb
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|&id| id > 0)
        else {
            return Ok(());
        };
        let Some(tmdb) = &self.tmdb else {
            return Ok(());
        };
        let Some(season_numbers) = tmdb
            .series_season_numbers(tmdb_id, one.language.as_deref())
            .await
        else {
            return Ok(());
        };
        let grace = settings.grace_period_days.max(0);
        let prune_aged_out = !settings.import_missing;

        // `GetExistingEpisodes`: every (season, episode) the series has, the
        // TMDb ids its files carry, and its own placeholders to keep in sync.
        let mut keys: HashSet<(i64, i64)> = HashSet::new();
        let mut physical_keys: HashSet<(i64, i64)> = HashSet::new();
        let mut physical_tmdb: HashSet<String> = HashSet::new();
        let mut ours: Vec<((i64, i64), usize)> = Vec::new();
        let mut doomed: Vec<(usize, &'static str)> = Vec::new();
        for (i, episode) in episodes.iter().enumerate() {
            if should_prune(
                episode.facts(),
                prune_aged_out,
                today,
                grace,
                settings.import_specials,
            ) {
                doomed.push((i, "no longer upcoming and missing episodes are disabled"));
                continue;
            }
            if !episode.row.is_virtual_item
                && let Some(id) = &episode.tmdb_id
            {
                physical_tmdb.insert(id.clone());
            }
            if let Some(key) = episode.key() {
                keys.insert(key);
                if episode.ours() {
                    ours.push((key, i));
                } else if !episode.row.is_virtual_item {
                    physical_keys.insert(key);
                }
            }
        }
        let mut updatable: HashMap<(i64, i64), usize> = HashMap::new();
        for (key, i) in ours {
            if physical_keys.contains(&key) {
                doomed.push((i, "a physical episode now exists for this slot"));
            } else if episodes[i]
                .tmdb_id
                .as_ref()
                .is_some_and(|id| physical_tmdb.contains(id))
            {
                doomed.push((
                    i,
                    "the same episode is already present under another number",
                ));
            } else {
                updatable.insert(key, i);
            }
        }
        for (i, reason) in &doomed {
            let episode = &episodes[*i];
            tracing::debug!(
                item_id = %episode.row.id,
                series_name = series_row.name.as_deref().unwrap_or_default(),
                season_number = episode.row.parent_index_number,
                episode_number = episode.row.index_number,
                reason,
                "removing a virtual episode"
            );
        }
        let doomed_ids: Vec<Uuid> = doomed
            .iter()
            .filter_map(|(i, _)| episodes[*i].id())
            .collect();
        self.remove_episodes(&doomed_ids, changes).await?;

        let ancestors = self
            .persistence
            .scan_stored_links(&[one.id])
            .await?
            .and_then(|mut links| links.remove(&one.id))
            .map(|links| links.ancestors)
            .unwrap_or_default();
        let series = SeriesContext {
            row: &series_row,
            id: one.id,
            key,
            ancestors,
        };
        // A season number's own season: a physical one over a virtual one,
        // so a placeholder joins the season the library has. Ordered by
        // number, as upstream's dictionary keeps the series' order.
        let mut seasons: BTreeMap<i64, StoredSeason> = BTreeMap::new();
        for row in &season_rows {
            if let (Some(number), Ok(id)) = (row.index_number, Uuid::parse_str(&row.id)) {
                let replaces = seasons
                    .get(&number)
                    .is_none_or(|kept| kept.row.is_virtual_item && !row.is_virtual_item);
                if replaces {
                    seasons.insert(
                        number,
                        StoredSeason {
                            id,
                            name: row.name.clone(),
                            row: row.clone(),
                        },
                    );
                }
            }
        }
        let updatable_ids: Vec<Uuid> = updatable
            .values()
            .filter_map(|&i| episodes[i].id())
            .collect();
        let mut with_primary: HashSet<Uuid> = self
            .persistence
            .scan_stored_links(&updatable_ids)
            .await?
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, links)| {
                links
                    .images
                    .iter()
                    .any(|i| i.image_type == ImageType::Primary)
            })
            .map(|(id, _)| id)
            .collect();
        for number in season_numbers {
            let Some(details) = self
                .season_for_missing_episodes(
                    cache,
                    &series_row.id,
                    tmdb_id,
                    number,
                    one.language.as_deref(),
                )
                .await
            else {
                continue;
            };
            let season_number = i64::from(number);
            for tmdb_episode in &details.episodes {
                let episode_number = i64::from(tmdb_episode.episode_number);
                let premiere = premiere_date(tmdb_episode.air_date.as_deref());
                if !should_import_episode(
                    premiere,
                    today,
                    settings.import_unaired,
                    settings.import_missing,
                    number == 0,
                    settings.import_specials,
                ) {
                    continue;
                }
                let key = (season_number, episode_number);
                if let Some(&i) = updatable.get(&key) {
                    let season = self
                        .season_for_episode(&series, season_number, &details, &mut seasons, changes)
                        .await?;
                    let mut row = episodes[i].row.clone();
                    let mut edited = update_virtual_episode(&mut row, tmdb_episode, premiere);
                    let season_db = guid_to_db(season.id);
                    let reparented = !row
                        .parent_id
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case(&season_db));
                    if reparented {
                        row.parent_id = Some(season_db.clone());
                        row.season_id = Some(season_db);
                        row.season_name.clone_from(&season.name);
                        edited = true;
                    }
                    let Some(id) = episodes[i].id() else {
                        continue;
                    };
                    if edited {
                        self.persistence
                            .save_items(std::slice::from_ref(&row))
                            .await?;
                        if reparented {
                            self.persistence
                                .set_ancestors(id, &episode_ancestors(&series, season.id))
                                .await?;
                        }
                        changes.updated.push(id);
                    }
                    // The still of a placeholder created before images were
                    // fetched (`EnsureEpisodeImageAsync`).
                    if !with_primary.contains(&id)
                        && self.save_episode_still(id, tmdb_episode).await
                    {
                        with_primary.insert(id);
                    }
                    continue;
                }
                if !keys.insert(key) {
                    continue;
                }
                // The slot is free, but the episode may sit at another number
                // when the files follow a different order.
                if tmdb_episode
                    .tmdb_id
                    .is_some_and(|id| physical_tmdb.contains(&id.to_string()))
                {
                    continue;
                }
                let season = self
                    .season_for_episode(&series, season_number, &details, &mut seasons, changes)
                    .await?;
                let Some(id) = self
                    .add_virtual_episode(&series, &season, tmdb_episode, premiere)
                    .await?
                else {
                    continue;
                };
                changes.added.push(id);
                self.save_episode_still(id, tmdb_episode).await;
            }
        }
        self.align_virtual_season_sort_names(&mut seasons, changes)
            .await?;
        self.remove_emptied_seasons(items, &season_rows, &season_tmdb, changes)
            .await?;
        Ok(())
    }

    /// Season `number` of the series' TMDb record: the walk's cached
    /// response for it in `language`, else a request of its own (as after
    /// a scan longer than the cache keeps a season). [`None`] when TMDb has
    /// no such season or the request failed.
    async fn season_for_missing_episodes(
        &self,
        cache: &ArtworkCache,
        series_id: &str,
        tmdb_id: i64,
        number: i32,
        language: Option<&str>,
    ) -> Option<SeasonDetails> {
        match cache
            .season_details
            .get(&(series_id.to_owned(), number, language.map(str::to_owned)))
        {
            Some(SeasonLookup::Found(details)) => return Some(details.clone()),
            Some(SeasonLookup::Missing | SeasonLookup::Failed) => return None,
            None => {}
        }
        self.tmdb
            .as_ref()?
            .season_details_for_language(tmdb_id, number, language)
            .await
    }

    /// `GetOrCreateSeasonAsync`: the series' season `number`, created as a
    /// virtual season (refreshed by the season providers, as upstream's
    /// `season.RefreshMetadata`) when the library has nothing of it.
    async fn season_for_episode(
        &self,
        series: &SeriesContext<'_>,
        number: i64,
        details: &SeasonDetails,
        seasons: &mut BTreeMap<i64, StoredSeason>,
        changes: &mut Changes,
    ) -> Result<StoredSeason, ServiceError> {
        if let Some(season) = seasons.get(&number) {
            return Ok(season.clone());
        }
        let key = format!("{}Season{number}", series.id.simple());
        let Some(id) =
            item_type_lookup::derive_item_id_with(&self.id_derivation, BaseItemKind::Season, &key)
        else {
            return Err(ServiceError::backend("no season item type"));
        };
        tracing::debug!(
            item_id = %id,
            series_name = series.row.name.as_deref().unwrap_or_default(),
            season_number = number,
            "creating a virtual season"
        );
        let row = BaseItemEntity {
            id: guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::Season)
                .unwrap_or_default()
                .to_owned(),
            name: details.name.clone().filter(|n| !n.is_empty()),
            index_number: Some(number),
            is_virtual_item: true,
            is_folder: true,
            series_id: Some(guid_to_db(series.id)),
            series_name: series.row.name.clone(),
            series_presentation_unique_key: Some(series.key.clone()),
            parent_id: Some(guid_to_db(series.id)),
            top_parent_id: series.row.top_parent_id.clone(),
            date_created: Some(Utc::now()),
            ..BaseItemEntity::default()
        };
        self.persistence
            .save_items(std::slice::from_ref(&row))
            .await?;
        let mut ancestors = series.ancestors.clone();
        ancestors.push(series.id);
        self.persistence.set_ancestors(id, &ancestors).await?;
        if let Some(tmdb_id) = details.tmdb_id {
            self.persistence
                .save_provider_id(id, TMDB, &tmdb_id.to_string())
                .await?;
        }
        if let Some(providers) = &self.metadata_savers
            && let Err(err) = providers
                .refresh_single_item(id, &MetadataRefreshOptions::default())
                .await
        {
            tracing::warn!(item_id = %id, %err, "could not refresh the virtual season");
        }
        // The refresh may have named it.
        let stored = match &self.item_repository {
            Some(items) => items.retrieve_item(id).await?.unwrap_or(row),
            None => row,
        };
        let season = StoredSeason {
            id,
            name: stored.name.clone(),
            row: stored,
        };
        changes.added.push(id);
        seasons.insert(number, season.clone());
        Ok(season)
    }

    /// `AddVirtualEpisode`: a path-less episode of `season`, keyed by series,
    /// season and episode number as upstream keys it, with TMDb's id.
    async fn add_virtual_episode(
        &self,
        series: &SeriesContext<'_>,
        season: &StoredSeason,
        tmdb: &EpisodeDetails,
        premiere: Option<DateTime<Utc>>,
    ) -> Result<Option<Uuid>, ServiceError> {
        let season_number = season.row.index_number.unwrap_or_default();
        let episode_number = i64::from(tmdb.episode_number);
        let key = format!(
            "{}Season{season_number}Episode{episode_number}",
            series.id.simple()
        );
        let Some(id) =
            item_type_lookup::derive_item_id_with(&self.id_derivation, BaseItemKind::Episode, &key)
        else {
            return Ok(None);
        };
        tracing::debug!(
            item_id = %id,
            series_name = series.row.name.as_deref().unwrap_or_default(),
            season_number,
            episode_number,
            "creating a virtual episode"
        );
        let row = BaseItemEntity {
            id: guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::Episode)
                .unwrap_or_default()
                .to_owned(),
            name: tmdb.name.clone(),
            overview: tmdb.overview.clone(),
            index_number: Some(episode_number),
            parent_index_number: Some(season_number),
            is_virtual_item: true,
            premiere_date: premiere,
            production_year: premiere.map(|p| i64::from(p.year())),
            media_type: Some("Video".to_owned()),
            season_id: Some(guid_to_db(season.id)),
            season_name: season.name.clone(),
            series_id: Some(guid_to_db(series.id)),
            series_name: series.row.name.clone(),
            series_presentation_unique_key: Some(series.key.clone()),
            parent_id: Some(guid_to_db(season.id)),
            top_parent_id: series.row.top_parent_id.clone(),
            date_created: Some(Utc::now()),
            ..BaseItemEntity::default()
        };
        self.persistence
            .save_items(std::slice::from_ref(&row))
            .await?;
        self.persistence
            .set_ancestors(id, &episode_ancestors(series, season.id))
            .await?;
        if let Some(tmdb_id) = tmdb.tmdb_id.filter(|&id| id > 0) {
            self.persistence
                .save_provider_id(id, TMDB, &tmdb_id.to_string())
                .await?;
        }
        Ok(Some(id))
    }

    /// `EnsureEpisodeImageAsync`: TMDb's still as the episode's Primary, so
    /// it does not fall back to the season's or series' image. Whether one
    /// was saved; a failed download is logged and left for the next refresh.
    async fn save_episode_still(&self, id: Uuid, tmdb: &EpisodeDetails) -> bool {
        let (Some(providers), Some(url)) = (&self.metadata_savers, tmdb.still_url.as_deref())
        else {
            return false;
        };
        match providers
            .save_image_from_url(id, url, ImageType::Primary, None)
            .await
        {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(item_id = %id, %err, "could not download the virtual episode's still");
                false
            }
        }
    }

    /// `AlignVirtualSeasonSortNamesAsync`: each virtual season's forced sort
    /// name mirrors a physical sibling's name-based one
    /// ([`season_sort_name_template`]).
    async fn align_virtual_season_sort_names(
        &self,
        seasons: &mut BTreeMap<i64, StoredSeason>,
        changes: &mut Changes,
    ) -> Result<(), ServiceError> {
        let facts: Vec<SeasonFacts<'_>> = seasons
            .values()
            .map(|s| SeasonFacts {
                virtual_item: s.row.is_virtual_item,
                number: s.row.index_number,
                forced_sort_name: s.row.forced_sort_name.as_deref(),
            })
            .collect();
        let Some(template) = season_sort_name_template(&facts) else {
            return Ok(());
        };
        for season in seasons.values_mut() {
            let Some(number) = season
                .row
                .index_number
                .filter(|_| season.row.is_virtual_item)
            else {
                continue;
            };
            let desired = template(number);
            if season.row.forced_sort_name.as_deref() == Some(desired.as_str()) {
                continue;
            }
            tracing::debug!(
                item_id = %season.id,
                season_number = number,
                sort_name = %desired,
                "aligning a virtual season's sort name"
            );
            season.row.forced_sort_name = Some(desired);
            self.persistence
                .save_items(std::slice::from_ref(&season.row))
                .await?;
            changes.updated.push(season.id);
        }
        Ok(())
    }

    /// Deletes `ids` (rows only: a virtual episode has no file).
    async fn remove_episodes(
        &self,
        ids: &[Uuid],
        changes: &mut Changes,
    ) -> Result<(), ServiceError> {
        if ids.is_empty() {
            return Ok(());
        }
        let removed = self.delete_announced(ids).await?;
        self.remove_item_art(&removed).await;
        changes.removed.extend(removed);
        Ok(())
    }

    /// The internal metadata folders of deleted rows `ids` (a placeholder's
    /// downloaded still), as `DeleteItem` removes an item's metadata path.
    async fn remove_item_art(&self, ids: &[Uuid]) {
        let Some(root) = self.metadata_dir.as_ref().map(DirectoryPath::resolve) else {
            return;
        };
        let folders: Vec<std::path::PathBuf> = ids
            .iter()
            .flat_map(|id| crate::item_folders::item_art_folders(&root, *id))
            .collect();
        let removed = tokio::task::spawn_blocking(move || {
            for folder in folders {
                match std::fs::remove_dir_all(&folder) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => tracing::warn!(
                        path = %folder.display(),
                        %err,
                        "could not delete a removed virtual item's metadata folder"
                    ),
                }
            }
        })
        .await;
        if let Err(err) = removed {
            tracing::error!(%err, "deleting virtual item metadata folders panicked");
        }
    }

    /// `RemoveObsoleteSeasons`' empty-season rule over this provider's own
    /// virtual seasons (path-less, with a TMDb id), as the same series
    /// refresh deletes them upstream: one with no child left goes. Whether
    /// it has one is asked of the database right before the delete — a
    /// season's delete takes every row under it, so nothing the pass read
    /// earlier decides it.
    async fn remove_emptied_seasons(
        &self,
        items: &dyn ItemRepository,
        season_rows: &[BaseItemEntity],
        season_tmdb: &HashMap<String, Option<String>>,
        changes: &mut Changes,
    ) -> Result<(), ServiceError> {
        let candidates = season_rows
            .iter()
            .filter(|s| s.is_virtual_item && s.path.as_deref().is_none_or(str::is_empty))
            .filter(|s| season_tmdb.get(&s.id).is_some_and(Option::is_some))
            .filter_map(|s| Uuid::parse_str(&s.id).ok());
        let mut empty = Vec::new();
        for id in candidates {
            // Anything the delete would take with it (the season's own
            // closure), and any row that still names it among its ancestors
            // (a general query's hidden versions and owned rows included).
            let has_child = self.persistence.deletion_closure(&[id]).await?.len() > 1
                || !items
                    .get_item_ids(&InternalItemsQuery {
                        ancestor_ids: vec![id],
                        include_owned_items: true,
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await?
                    .is_empty();
            if !has_child {
                tracing::debug!(item_id = %id, "removing an empty virtual season");
                empty.push(id);
            }
        }
        self.remove_episodes(&empty, changes).await
    }

    /// Reports the episodes and seasons the provider created and the ones
    /// it updated to the per-user `LibraryChanged` push, as upstream's
    /// `CreateItem` and `UpdateToRepositoryAsync` raise `ItemAdded` and
    /// `ItemUpdated` (its removals were reported as they were deleted). The
    /// notifier passes on what `FilterItem` admits: the seasons, not the
    /// pathless episodes.
    async fn announce_missing_episode_changes(
        &self,
        items: &dyn ItemRepository,
        changes: &Changes,
    ) {
        let Some(notifier) = &self.change_notifier else {
            return;
        };
        for (ids, added) in [(&changes.added, true), (&changes.updated, false)] {
            if ids.is_empty() {
                continue;
            }
            match items.retrieve_items(ids).await {
                Ok(rows) if added => notifier.record_added(&rows),
                Ok(rows) => notifier.record_updated(&rows),
                Err(err) => tracing::warn!(
                    %err,
                    count = ids.len(),
                    "could not read the missing and unaired episodes to announce them"
                ),
            }
        }
    }
}

/// An episode's ancestors: the series', the series and the season.
fn episode_ancestors(series: &SeriesContext<'_>, season: Uuid) -> Vec<Uuid> {
    let mut ancestors = series.ancestors.clone();
    ancestors.push(series.id);
    ancestors.push(season);
    ancestors
}

#[cfg(test)]
mod tests {
    //! `tests/Jellyfin.Providers.Tests/Tmdb/TmdbMissingEpisodeProviderTests.cs`
    //! (v12.2), transliterated.

    use chrono::TimeZone as _;
    use rstest::rstest;

    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 20).expect("date")
    }

    fn at(offset_days: i64) -> DateTime<Utc> {
        Utc.from_utc_datetime(&today().and_hms_opt(0, 0, 0).expect("time"))
            + chrono::Duration::days(offset_days)
    }

    #[rstest]
    // No air date -> never imported, regardless of options.
    #[case(None, true, true, false, false, false)]
    #[case(None, false, false, false, false, false)]
    // Future (unaired) episodes are gated by the unaired option.
    #[case(Some(5), true, false, false, false, true)]
    #[case(Some(5), false, false, false, false, false)]
    #[case(Some(5), false, true, false, false, false)]
    // Today counts as unaired.
    #[case(Some(0), true, false, false, false, true)]
    #[case(Some(0), false, false, false, false, false)]
    // Past (already aired) episodes are gated by the missing option.
    #[case(Some(-5), false, true, false, false, true)]
    #[case(Some(-5), false, false, false, false, false)]
    #[case(Some(-5), true, false, false, false, false)]
    // Specials are never imported when the specials option is off.
    #[case(Some(5), true, false, true, false, false)]
    #[case(Some(-5), false, true, true, false, false)]
    // Specials follow the normal air-date gating when the specials option is on.
    #[case(Some(5), true, false, true, true, true)]
    #[case(Some(5), false, false, true, true, false)]
    #[case(Some(-5), false, true, true, true, true)]
    #[case(Some(-5), false, false, true, true, false)]
    fn should_import_episode_respects_air_date_and_options(
        #[case] day_offset: Option<i64>,
        #[case] import_unaired: bool,
        #[case] import_missing: bool,
        #[case] is_special: bool,
        #[case] import_specials: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(
            should_import_episode(
                day_offset.map(at),
                today(),
                import_unaired,
                import_missing,
                is_special,
                import_specials
            ),
            expected
        );
    }

    fn virtual_episode(offset: i64, with_tmdb_id: bool, season: Option<i64>) -> PruneFacts {
        PruneFacts {
            virtual_item: true,
            has_tmdb_id: with_tmdb_id,
            season,
            premiere: Some(at(offset)),
        }
    }

    #[rstest]
    // An aged-out virtual TMDb episode.
    #[case(virtual_episode(-1, true, None), true, 0, true, true)]
    // Not in pruning mode.
    #[case(virtual_episode(-1, true, None), false, 0, true, false)]
    // Still upcoming.
    #[case(virtual_episode(1, true, None), true, 0, true, false)]
    // A virtual episode from another provider (no TMDb id) is left alone.
    #[case(virtual_episode(-1, false, None), true, 0, true, false)]
    // A physical episode.
    #[case(PruneFacts { virtual_item: false, ..virtual_episode(-1, true, None) }, true, 0, true, false)]
    // Aired within the grace period.
    #[case(virtual_episode(-2, true, None), true, 7, true, false)]
    // Aired beyond the grace period.
    #[case(virtual_episode(-10, true, None), true, 7, true, true)]
    // A special with specials disabled goes, even outside pruning mode.
    #[case(virtual_episode(5, true, Some(0)), false, 7, false, true)]
    // A special with specials enabled follows the normal rules.
    #[case(virtual_episode(5, true, Some(0)), true, 7, true, false)]
    fn should_prune_follows_the_options(
        #[case] episode: PruneFacts,
        #[case] prune_aged_out: bool,
        #[case] grace_days: i32,
        #[case] import_specials: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(
            should_prune(
                episode,
                prune_aged_out,
                today(),
                grace_days,
                import_specials
            ),
            expected
        );
    }

    #[test]
    fn premiere_date_of_no_air_date_is_none() {
        assert_eq!(premiere_date(None), None);
    }

    #[test]
    fn premiere_date_is_the_air_date() {
        let premiere = premiere_date(Some("2026-07-28")).expect("date");
        assert_eq!(
            premiere.date_naive(),
            NaiveDate::from_ymd_opt(2026, 7, 28).expect("date")
        );
    }

    fn tmdb_episode(name: Option<&str>, overview: Option<&str>) -> EpisodeDetails {
        EpisodeDetails {
            tmdb_id: None,
            episode_number: 1,
            name: name.map(str::to_owned),
            overview: overview.map(str::to_owned),
            still_url: None,
            air_date: None,
            vote_average: None,
        }
    }

    #[test]
    fn update_virtual_episode_replaces_a_placeholder_title() {
        let mut row = BaseItemEntity {
            name: Some("Episode 14".to_owned()),
            ..BaseItemEntity::default()
        };
        assert!(update_virtual_episode(
            &mut row,
            &tmdb_episode(Some("The Real Title"), None),
            None
        ));
        assert_eq!(row.name.as_deref(), Some("The Real Title"));
    }

    #[test]
    fn update_virtual_episode_without_changes_returns_false() {
        let mut row = BaseItemEntity {
            name: Some("Same".to_owned()),
            overview: Some("Description".to_owned()),
            premiere_date: Some(at(0)),
            ..BaseItemEntity::default()
        };
        assert!(!update_virtual_episode(
            &mut row,
            &tmdb_episode(Some("Same"), Some("Description")),
            Some(at(0))
        ));
    }

    #[test]
    fn update_virtual_episode_keeps_values_tmdb_leaves_empty() {
        let mut row = BaseItemEntity {
            name: Some("Existing".to_owned()),
            overview: Some("Existing overview".to_owned()),
            ..BaseItemEntity::default()
        };
        assert!(!update_virtual_episode(
            &mut row,
            &tmdb_episode(Some(""), None),
            None
        ));
        assert_eq!(row.name.as_deref(), Some("Existing"));
        assert_eq!(row.overview.as_deref(), Some("Existing overview"));
    }

    #[test]
    fn update_virtual_episode_takes_a_rescheduled_air_date_and_its_year() {
        let mut row = BaseItemEntity {
            name: Some("X".to_owned()),
            premiere_date: Some(
                Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
                    .single()
                    .expect("date"),
            ),
            ..BaseItemEntity::default()
        };
        let rescheduled = premiere_date(Some("2026-08-15"));
        assert!(update_virtual_episode(
            &mut row,
            &tmdb_episode(Some("X"), None),
            rescheduled
        ));
        assert_eq!(row.premiere_date, rescheduled);
        assert_eq!(row.production_year, Some(2026));
    }

    fn physical(number: i64, forced: Option<&str>) -> SeasonFacts<'_> {
        SeasonFacts {
            virtual_item: false,
            number: Some(number),
            forced_sort_name: forced,
        }
    }

    fn virtual_season(number: i64) -> SeasonFacts<'static> {
        SeasonFacts {
            virtual_item: true,
            number: Some(number),
            forced_sort_name: None,
        }
    }

    #[test]
    fn no_name_sorted_physical_season_gives_no_template() {
        assert!(season_sort_name_template(&[physical(1, None), virtual_season(3)]).is_none());
    }

    #[test]
    fn the_template_mirrors_the_siblings_convention_and_swaps_the_number() {
        let template =
            season_sort_name_template(&[physical(1, Some("Season 01")), virtual_season(3)])
                .expect("template");
        assert_eq!(template(3), "Season 03");
        assert_eq!(template(12), "Season 12");
    }

    #[test]
    fn the_template_keeps_a_non_english_token() {
        let template =
            season_sort_name_template(&[physical(1, Some("Staffel 1")), virtual_season(2)])
                .expect("template");
        assert_eq!(template(2), "Staffel 2");
    }

    #[test]
    fn a_sibling_without_digits_gives_no_template() {
        assert!(
            season_sort_name_template(&[physical(1, Some("Miniseries")), virtual_season(2)])
                .is_none()
        );
    }

    #[test]
    fn a_virtual_season_is_never_the_reference() {
        let forced_virtual = SeasonFacts {
            forced_sort_name: Some("Season 03"),
            ..virtual_season(3)
        };
        assert!(season_sort_name_template(&[physical(1, None), forced_virtual]).is_none());
    }
}
