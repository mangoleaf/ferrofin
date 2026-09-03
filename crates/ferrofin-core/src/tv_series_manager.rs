//! [`FerrofinTvSeriesManager`] — the concrete [`TvSeriesManager`] (Next Up queue).
//!
//! Port of `Emby.Server.Implementations.TV.TVSeriesManager` (v12). It computes a
//! user's "Next Up" episode list by delegating the per-series next-up algorithm
//! to the [`NextUpService`](ferrofin_traits::persistence::NextUpService), deciding
//! each series' pick with `DetermineNextEpisode` (the specials merge and the
//! resumable rule), ordering the picks by the last watched episode's play date,
//! and then projecting the episode rows to
//! [`BaseItemDto`](ferrofin_model::dto::BaseItemDto) through the injected
//! [`DtoService`](ferrofin_traits::dto::DtoService), paginating with the same
//! `start_index`/`limit`/`enable_total_record_count` semantics as C# `GetResult`.
//!
//! Port rules applied:
//! - The two C# `GetNextUp` overloads collapse to the single trait method
//!   [`TvSeriesManager::get_next_up`]; the explicit `BaseItem[] parentsFolders`
//!   overload is folded into the query's [`NextUpQuery::parent_id`].
//! - The C# `query.User` domain object is resolved from
//!   [`NextUpQuery::user_id`] through the injected
//!   [`UserManager`](ferrofin_traits::library::UserManager); the derived
//!   [`UserEntity`](ferrofin_db::entities::users::UserEntity) then rides the
//!   [`InternalItemsQuery`] the way C# builds `new InternalItemsQuery(user)`.
//! - The un-ported `Series`/`Episode` domain tree is not reconstructed: the
//!   presentation key for a `series_id` is read straight off the persisted row
//!   (`PresentationUniqueKey ?? Id`, matching
//!   `Series.GetPresentationUniqueKey()`), and the `IUserDataManager` reads
//!   `DetermineNextEpisode` / `GetMostRecentlyPlayedVersion` make per episode
//!   are answered from the batch result's
//!   [`user_data`](ferrofin_traits::persistence::NextUpEpisodeBatchResult::user_data),
//!   which the service fills from the same projection that picked the rows.
//! - `DisplaySpecialsWithinSeasons` is read from the injected
//!   [`ServerConfigurationManager`](ferrofin_traits::configuration::ServerConfigurationManager),
//!   as in C#.
//! - Synchronous C# methods become `async fn -> Result<_, ServiceError>` (the
//!   impl paginates the database via its injected repositories).
//!
//! TODO(next-up-preferred-version): `GetPreferredVersion` — continuing in the
//! alternate version the user has been watching, matched by media-source
//! *name* — is not ported yet. It needs v12's version naming
//! (`Video.GetMediaSourceName` strips the shared file-name prefix so a version
//! is named `1080p`, not `S01E02 - 1080p`), which lives in the media-source
//! manager; port that naming first, then swap the pick for the version of the
//! next episode whose source name equals the played version's. Until then the
//! pick is always the primary episode row.

use std::cmp::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::{NextUpEpisodeBatchResult, NextUpService};
use ferrofin_traits::tv::{NextUpQuery, TvSeriesManager};

use crate::item_type_lookup::kind_from_type_name;
use crate::kinds;

/// The concrete TV-series (Next Up) manager.
///
/// Holds its collaborating managers behind `Arc<dyn _>` so they can be injected
/// at the composition root; this crate depends only on the traits.
#[derive(Clone)]
pub struct FerrofinTvSeriesManager {
    user_manager: Arc<dyn UserManager>,
    library_manager: Arc<dyn LibraryManager>,
    next_up_service: Arc<dyn NextUpService>,
    dto_service: Arc<dyn DtoService>,
    configuration_manager: Arc<dyn ServerConfigurationManager>,
}

impl std::fmt::Debug for FerrofinTvSeriesManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinTvSeriesManager")
            .finish_non_exhaustive()
    }
}

/// A picked episode and the date it is ordered by: C#'s
/// `(DateTime LastWatchedDate, Episode Episode)` tuple.
type DatedPick = (DateTime<Utc>, BaseItemEntity);

impl FerrofinTvSeriesManager {
    /// Creates a TV-series manager from its injected collaborators.
    #[must_use]
    pub fn new(
        user_manager: Arc<dyn UserManager>,
        library_manager: Arc<dyn LibraryManager>,
        next_up_service: Arc<dyn NextUpService>,
        dto_service: Arc<dyn DtoService>,
        configuration_manager: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        Self {
            user_manager,
            library_manager,
            next_up_service,
            dto_service,
            configuration_manager,
        }
    }

    /// The presentation unique key of a series row: its explicit
    /// `PresentationUniqueKey` when set, else its id (mirrors
    /// `Series.GetPresentationUniqueKey()`).
    fn series_presentation_key(series: &BaseItemEntity) -> String {
        series
            .presentation_unique_key
            .clone()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| series.id.clone())
    }

    /// Resolves the parents the next-up scan is scoped to.
    ///
    /// When `parent_id` is set, that single parent is the scope (the C#
    /// `parents = [parent]` branch). Otherwise the scope is the user's library
    /// folders: `GetUserRootFolder().GetChildren(user, true)` — the user root's
    /// children the user may see — kept to `Folder`s and minus the views the
    /// user excluded from "latest" (`PreferenceKind.LatestItemExcludes`).
    ///
    /// These are the 3–7 library rows, never the library's folders at large:
    /// the keys statement's `TopParentId IN (…)` is evaluated per candidate
    /// row, and asking for every folder (seasons, albums, artists — 1,975 on
    /// the bench fixture) against the retired `CROSS JOIN` shape made that
    /// 10 M index probes and a 1.4 s home screen. The service maps each
    /// collection folder to its physical folders, as
    /// `LibraryManager.GetNextUpSeriesKeys` does.
    async fn resolve_parents(
        &self,
        parent_id: Option<uuid::Uuid>,
        user: &UserEntity,
    ) -> Result<Vec<uuid::Uuid>, ServiceError> {
        if let Some(parent) = parent_id {
            // Only scope to it if it actually exists (C# `parent is not null`).
            if self.library_manager.get_item_by_id(parent).await?.is_some() {
                return Ok(vec![parent]);
            }
            return Ok(Vec::new());
        }

        // `user_root_children` is the repository's `GetChildren(user, true)`
        // branch: the user root's own rows plus the aggregate's virtual
        // children, filtered by the user's blocked-/enabled-folders. It needs
        // the repository's root ids, which the server always injects
        // (`with_root_ids`). On a repository built without them the branch is
        // inert and the query is scoped to the user's libraries instead — no
        // view row is *under* a library, so with the type list below that
        // answers no parents and Next Up is empty, rather than every folder
        // in the library.
        let query = InternalItemsQuery {
            user: Some(user.clone()),
            user_root_children: true,
            include_item_types: vec![
                BaseItemKind::CollectionFolder,
                BaseItemKind::UserView,
                BaseItemKind::PlaylistsFolder,
                BaseItemKind::ManualPlaylistsFolder,
            ],
            ..InternalItemsQuery::default()
        };
        let children = self.library_manager.query_items(&query).await?.items;

        // `user.GetPreferenceValues<Guid>(PreferenceKind.LatestItemExcludes)`.
        let excludes = self
            .user_manager
            .get_user_dto(user, None)
            .await?
            .configuration
            .map(|c| c.latest_items_excludes)
            .unwrap_or_default();

        Ok(children
            .iter()
            .filter(|row| kind_from_type_name(&row.type_).is_some_and(kinds::is_folder))
            .filter_map(|row| uuid::Uuid::parse_str(&row.id).ok())
            .filter(|id| !excludes.contains(id))
            .collect())
    }

    /// Runs the batched next-up algorithm for a set of series keys and returns
    /// the picked episode rows ordered by their last-watched date, newest first.
    ///
    /// Port of `GetNextUpBatched`: it asks the [`NextUpService`] for each
    /// series' batch result, decides the pick with [`determine_next_episode`]
    /// (and, when rewatching is enabled, the "next played" pick), dates each
    /// pick by the last watched episode's most recent play date
    /// (`GetMostRecentlyPlayedVersion`), and sorts the whole list by that date.
    async fn next_up_batched(
        &self,
        request: &NextUpQuery,
        user: &UserEntity,
        series_keys: &[String],
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if series_keys.is_empty() {
            return Ok(Vec::new());
        }

        let include_specials = self
            .configuration_manager
            .configuration()
            .await?
            .display_specials_within_seasons;
        let include_rewatching = request.enable_rewatching;

        let query = InternalItemsQuery {
            user: Some(user.clone()),
            dto_options: dto_options.clone(),
            ..InternalItemsQuery::default()
        };

        let batch = self
            .next_up_service
            .get_next_up_episodes_batch(&query, series_keys, include_specials, include_rewatching)
            .await?;

        // Appended in series-key order (last-played-date descending from
        // `get_next_up_series_keys`), the fresh next-up before the rewatching
        // pick, exactly as the C# `nextUpList` is built.
        let mut next_up_list: Vec<DatedPick> = Vec::new();
        for key in series_keys {
            let Some(result) = batch.get(key) else {
                continue;
            };
            if let Some(next) =
                determine_next_episode(result, include_specials, request.enable_resumable, false)
            {
                next_up_list.push((last_watched_date(result, false), next));
            }
            if include_rewatching
                && let Some(next_played) =
                    determine_next_episode(result, include_specials, false, true)
            {
                next_up_list.push((last_watched_date(result, true), next_played));
            }
        }

        // `OrderByDescending` is stable, so ties keep the series-key order.
        next_up_list.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
        Ok(next_up_list.into_iter().map(|(_, ep)| ep).collect())
    }

    /// Paginates the selected episode rows into a DTO query result.
    ///
    /// Port of the static `GetResult`: the total is the pick count when
    /// `enable_total_record_count`, else `0` — never the page length — then
    /// `start_index`/`limit` are applied.
    async fn to_result(
        &self,
        episodes: Vec<BaseItemEntity>,
        request: &NextUpQuery,
        user: &UserEntity,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let total_count = if request.enable_total_record_count {
            i32::try_from(episodes.len()).unwrap_or(i32::MAX)
        } else {
            0
        };

        let start = request
            .start_index
            .and_then(|s| usize::try_from(s).ok())
            .unwrap_or(0);
        let mut page: Vec<BaseItemEntity> = episodes.into_iter().skip(start).collect();
        if let Some(limit) = request.limit
            && limit > 0
        {
            page.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let dtos = self
            .dto_service
            .get_base_item_dtos(&page, options, Some(user), None, true)
            .await?;

        Ok(QueryResult::new(
            request.start_index,
            Some(total_count),
            dtos,
        ))
    }
}

/// Port of `TVSeriesManager.DetermineNextEpisode`: the series' pick, after the
/// specials merge and the resumable rule.
///
/// With `include_played` the rewatching pair (`NextPlayedForRewatching` /
/// `LastWatchedForRewatching`) is decided instead of the fresh pair. When
/// specials are shown within seasons and the series has any, the specials
/// that air at a known position, the last watched episode and the pick are
/// sorted in aired order and the pick becomes the first row after the last
/// watched one (skipping played rows unless `include_played`). Then, unless
/// `include_resumable`, a pick with resume progress on any of its versions is
/// dropped.
fn determine_next_episode(
    result: &NextUpEpisodeBatchResult,
    include_specials: bool,
    include_resumable: bool,
    include_played: bool,
) -> Option<BaseItemEntity> {
    let (mut next, last_watched) = if include_played {
        (
            result.next_played_for_rewatching.as_ref(),
            result.last_watched_for_rewatching.as_ref(),
        )
    } else {
        (result.next_up.as_ref(), result.last_watched.as_ref())
    };

    if include_specials && !result.specials.is_empty() {
        let mut considered: Vec<(AiredOrder, &BaseItemEntity)> = result
            .specials
            .iter()
            .map(|ep| (AiredOrder::of(ep), ep))
            .filter(|(order, _)| {
                order.airs_before_season.is_some() || order.airs_after_season.is_some()
            })
            .collect();
        considered.extend(last_watched.map(|ep| (AiredOrder::of(ep), ep)));
        considered.extend(next.map(|ep| (AiredOrder::of(ep), ep)));

        if !considered.is_empty() {
            // `LibraryManager.Sort(…, AiredEpisodeOrder, Ascending)` — LINQ's
            // `OrderBy`: stable, and tolerant of the comparer not being a
            // total order (an unnumbered episode compares equal to a special
            // that airs before an episode of its season while ordering
            // strictly against its numbered siblings). `slice::sort_by` may
            // panic on such a comparator since Rust 1.81, so the handful of
            // rows is insertion-sorted instead.
            stable_sort_by(&mut considered, |a, b| a.0.compare(&b.0));
            let mut sorted: Box<dyn Iterator<Item = &BaseItemEntity>> =
                Box::new(considered.into_iter().map(|(_, ep)| ep));
            if let Some(last) = last_watched {
                sorted = Box::new(sorted.skip_while(|ep| ep.id != last.id).skip(1));
            }
            if !include_played {
                sorted = Box::new(
                    sorted.filter(|ep| !result.user_data.get(&ep.id).is_some_and(|ud| ud.played)),
                );
            }
            next = sorted.next();
        }
    }

    let next = next?;
    if !include_resumable
        && result
            .user_data
            .get(&next.id)
            .is_some_and(|ud| ud.playback_position_ticks > 0)
    {
        // The resume progress may live on an alternate version — the
        // service's facts already span `GetAllVersions()`.
        return None;
    }
    Some(next.clone())
}

/// A stable insertion sort that never panics, whatever the comparator does:
/// LINQ's `OrderBy` semantics for a comparer that is not a total order. The
/// inputs are a series' handful of positioned specials plus two rows.
fn stable_sort_by<T>(items: &mut [T], mut compare: impl FnMut(&T, &T) -> Ordering) {
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 && compare(&items[j - 1], &items[j]) == Ordering::Greater {
            items.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// The date a pick is ordered by — `GetNextUpBatched`'s `lastWatchedDate`:
/// `DateTime.MinValue` when the series has no last watched episode, else the
/// most recently played version's `LastPlayedDate`, or `MinValue + 1 day` when
/// no version carries a date (a series that *was* watched sorts above one that
/// was not).
fn last_watched_date(result: &NextUpEpisodeBatchResult, rewatching: bool) -> DateTime<Utc> {
    let last_watched = if rewatching {
        result.last_watched_for_rewatching.as_ref()
    } else {
        result.last_watched.as_ref()
    };
    let min = ferrofin_model::json::datetime::dotnet_min();
    let Some(last) = last_watched else {
        return min;
    };
    result
        .user_data
        .get(&last.id)
        .and_then(|ud| ud.last_played_date)
        .unwrap_or_else(|| min + chrono::Duration::days(1))
}

/// The fields `AiredEpisodeOrderComparer` reads off an episode.
///
/// The `Airs*` numbers are not `BaseItems` columns: Jellyfin serializes them
/// into the row's `Data` blob, so they are parsed from it — only for specials,
/// the only rows whose `Airs*` the comparer consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AiredOrder {
    season: Option<i64>,
    episode: Option<i64>,
    premiere: Option<DateTime<Utc>>,
    airs_before_season: Option<i64>,
    airs_after_season: Option<i64>,
    airs_before_episode: Option<i64>,
}

impl AiredOrder {
    /// Reads an episode row's aired-order fields.
    fn of(episode: &BaseItemEntity) -> Self {
        let mut order = Self {
            season: episode.parent_index_number,
            episode: episode.index_number,
            premiere: episode.premiere_date,
            airs_before_season: None,
            airs_after_season: None,
            airs_before_episode: None,
        };
        if episode.parent_index_number == Some(0)
            && let Some(blob) = episode
                .data
                .as_deref()
                .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        {
            let number = |key: &str| blob.get(key).and_then(serde_json::Value::as_i64);
            order.airs_before_season = number("AirsBeforeSeasonNumber");
            order.airs_after_season = number("AirsAfterSeasonNumber");
            order.airs_before_episode = number("AirsBeforeEpisodeNumber");
        }
        order
    }

    /// `(ParentIndexNumber ?? -1) == 0`.
    fn is_special(&self) -> bool {
        self.season.unwrap_or(-1) == 0
    }

    /// Port of `AiredEpisodeOrderComparer.Compare(Episode, Episode)`.
    fn compare(&self, other: &Self) -> Ordering {
        match (self.is_special(), other.is_special()) {
            (true, true) => self.special_value().cmp(&other.special_value()),
            (false, false) => self.compare_episodes(other),
            (false, true) => self.compare_episode_to_special(other),
            (true, false) => other.compare_episode_to_special(self).reverse(),
        }
    }

    /// `CompareEpisodeToSpecial(x = self, y = special)`.
    fn compare_episode_to_special(&self, special: &Self) -> Ordering {
        let x_season = self.season.unwrap_or(-1);
        let y_season = special
            .airs_after_season
            .or(special.airs_before_season)
            .unwrap_or(-1);
        if x_season != y_season {
            return x_season.cmp(&y_season);
        }
        // Special comes after the episode's season.
        if special.airs_after_season.is_some() {
            return Ordering::Less;
        }
        // Special comes before the season.
        let Some(y_episode) = special.airs_before_episode else {
            return Ordering::Greater;
        };
        // Can't really compare if this happens.
        let Some(x_episode) = self.episode else {
            return Ordering::Equal;
        };
        // Special comes before the episode it names.
        if x_episode == y_episode {
            return Ordering::Greater;
        }
        x_episode.cmp(&y_episode)
    }

    /// `GetSpecialCompareValue`: season, then airs-after, then the episode
    /// it airs before, then the special's own number.
    fn special_value(&self) -> i64 {
        let mut value = self
            .airs_after_season
            .or(self.airs_before_season)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000);
        if self.airs_after_season.is_some() {
            value = value.saturating_add(1_000_000);
        }
        value = value.saturating_add(self.airs_before_episode.unwrap_or(0).saturating_mul(1_000));
        value.saturating_add(self.episode.unwrap_or(0))
    }

    /// `CompareEpisodes`: `(season ?? -1) * 1000 + (episode ?? -1)`, then the
    /// premiere dates when both are known.
    fn compare_episodes(&self, other: &Self) -> Ordering {
        let value = |o: &Self| {
            o.season
                .unwrap_or(-1)
                .saturating_mul(1_000)
                .saturating_add(o.episode.unwrap_or(-1))
        };
        let by_number = value(self).cmp(&value(other));
        if by_number != Ordering::Equal {
            return by_number;
        }
        match (self.premiere, other.premiere) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => Ordering::Equal,
        }
    }
}

#[async_trait]
impl TvSeriesManager for FerrofinTvSeriesManager {
    async fn get_next_up(
        &self,
        query: &NextUpQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let user = self
            .user_manager
            .get_user_by_id(query.user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {}", query.user_id)))?;

        // Single-series path: resolve its presentation key and batch just it.
        // (`GetItemById(SeriesId) is Series` — when the id does not resolve to
        // a series, C# leaves presentationUniqueKey null and falls through to
        // the parent scan below.)
        if let Some(series_id) = query.series_id
            && let Some(series) = self.library_manager.get_item_by_id(series_id).await?
            && kind_from_type_name(&series.type_) == Some(BaseItemKind::Series)
        {
            let key = Self::series_presentation_key(&series);
            let episodes = self.next_up_batched(query, &user, &[key], options).await?;
            return self.to_result(episodes, query, &user, options).await;
        }

        // Library-wide path: find eligible series keys under the scoped parents.
        let parents = self.resolve_parents(query.parent_id, &user).await?;
        if parents.is_empty() {
            return Ok(QueryResult::new(query.start_index, Some(0), Vec::new()));
        }

        let cutoff = query
            .next_up_date_cutoff
            .unwrap_or_else(ferrofin_model::json::datetime::dotnet_min);

        // No limit: C# only limits the keys (`limit + 10`) on the single-series
        // overload, which never reaches this statement. Capping the keys here
        // capped the *series* considered before the picks were decided, and
        // the resumable/specials rules can drop any of them — 12 items where
        // v12 answers 16.
        let keys_query = InternalItemsQuery {
            user: Some(user.clone()),
            top_parent_ids: parents,
            ..InternalItemsQuery::default()
        };
        let series_keys = self
            .next_up_service
            .get_next_up_series_keys(&keys_query, cutoff)
            .await?;

        let episodes = self
            .next_up_batched(query, &user, &series_keys, options)
            .await?;
        self.to_result(episodes, query, &user, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::dto::BaseItemDto;
    use rstest::rstest;
    use uuid::Uuid;

    use ferrofin_traits::configuration::ServerConfigurationManager;
    use ferrofin_traits::dto::DtoService;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::library::{LibraryManager, UserManager};
    use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
    use ferrofin_traits::persistence::{
        NextUpEpisodeBatchResult, NextUpEpisodeUserData, NextUpService,
    };
    use ferrofin_traits::tv::{NextUpQuery, TvSeriesManager};

    use crate::item_type_lookup::stored_type_name;
    use crate::test_support::{seed_user, test_db};

    use super::{AiredOrder, FerrofinTvSeriesManager, determine_next_episode};

    // ── Minimal fakes for the injected collaborators ──
    //
    // `UserEntity` has no `Default`, so the fixtures seed a throwaway in-memory
    // `ferrofin-db` user and read it back; every item row is built in memory.

    /// Parses an RFC3339 timestamp.
    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid timestamp")
    }

    /// The stored `Type` name of a kind.
    fn type_name(kind: BaseItemKind) -> String {
        stored_type_name(kind).unwrap_or_default().to_owned()
    }

    /// An in-memory `Episode` row at `(season, episode)`.
    fn episode(id: Uuid, season: i64, index: i64) -> BaseItemEntity {
        BaseItemEntity {
            id: guid_to_db(id),
            type_: type_name(BaseItemKind::Episode),
            parent_index_number: Some(season),
            index_number: Some(index),
            ..BaseItemEntity::default()
        }
    }

    /// An in-memory row of any kind.
    fn item(id: Uuid, kind: BaseItemKind) -> BaseItemEntity {
        BaseItemEntity {
            id: guid_to_db(id),
            type_: type_name(kind),
            ..BaseItemEntity::default()
        }
    }

    /// A batch result whose next-up is `next`, last watched `last` (with the
    /// given play date), and which carries the given user-data facts.
    fn batch_result(
        last: Option<(BaseItemEntity, Option<DateTime<Utc>>)>,
        next: Option<BaseItemEntity>,
    ) -> NextUpEpisodeBatchResult {
        let mut result = NextUpEpisodeBatchResult::default();
        if let Some((row, date)) = last {
            result.user_data.insert(
                row.id.clone(),
                NextUpEpisodeUserData {
                    played: true,
                    playback_position_ticks: 0,
                    last_played_date: date,
                },
            );
            result.last_watched = Some(row);
        }
        if let Some(row) = next {
            result
                .user_data
                .insert(row.id.clone(), NextUpEpisodeUserData::default());
            result.next_up = Some(row);
        }
        result
    }

    struct FakeUserManager {
        user: UserEntity,
        latest_item_excludes: Vec<Uuid>,
    }
    #[async_trait]
    impl UserManager for FakeUserManager {
        async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
            Ok(vec![self.user.clone()])
        }
        async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![Uuid::parse_str(&self.user.id).unwrap()])
        }
        async fn initialize(&self) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
            Ok((self.user.id == guid_to_db(id)).then(|| self.user.clone()))
        }
        async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
            Ok(Some(self.user.clone()))
        }
        async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
            Ok(Some(self.user.clone()))
        }
        async fn rename_user(
            &self,
            _id: Uuid,
            _old_name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
            Ok(self.user.clone())
        }
        async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn change_password(
            &self,
            _user_id: Uuid,
            _new_password: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn authenticate_user(
            &self,
            _username: &str,
            _password: &str,
            _remote_endpoint: &str,
            _is_user_session: bool,
        ) -> Result<Option<UserEntity>, ServiceError> {
            Ok(Some(self.user.clone()))
        }
        async fn get_authentication_providers(
            &self,
        ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_password_reset_providers(
            &self,
        ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
            Ok(Vec::new())
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
                configuration: Some(ferrofin_model::configuration::UserConfiguration {
                    latest_items_excludes: self.latest_item_excludes.clone(),
                    ..ferrofin_model::configuration::UserConfiguration::default()
                }),
                ..ferrofin_model::dto::UserDto::default()
            })
        }
        async fn update_configuration(
            &self,
            _user_id: Uuid,
            _config: &ferrofin_model::configuration::UserConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_policy(
            &self,
            _user_id: Uuid,
            _policy: &ferrofin_model::users::UserPolicy,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// The library: items by id, and the rows `GetUserRootFolder()
    /// .GetChildren(user, true)` answers with.
    struct FakeLibraryManager {
        items: HashMap<Uuid, BaseItemEntity>,
        root_children: Vec<BaseItemEntity>,
    }
    #[async_trait]
    impl LibraryManager for FakeLibraryManager {
        async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            Ok(self.items.get(&id).cloned())
        }
        async fn get_item_images(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn query_items(
            &self,
            query: &InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            assert!(
                query.user_root_children && query.user.is_some(),
                "the parents must come from the user root's children, for the user"
            );
            Ok(ferrofin_model::querying::QueryResult::from_items(
                self.root_children.clone(),
            ))
        }
        async fn get_item_ids(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            unreachable!("the next-up scope is the root children, never an id scan")
        }
        async fn get_item_list(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(self.root_children.clone())
        }
        async fn get_latest_item_list(
            &self,
            _query: &InternalItemsQuery,
            _collection_type: ferrofin_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn create_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_item(
            &self,
            _id: Uuid,
            _options: &ferrofin_traits::options::DeleteOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_people(
            &self,
            _query: &ferrofin_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_people_names(
            &self,
            _query: &ferrofin_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
            Ok(ferrofin_model::dto::ItemCounts::default())
        }
        async fn get_genres(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_studios(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_artists(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_music_genres(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_album_artists(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_query_filters_legacy(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
            Ok(ferrofin_model::querying::QueryFiltersLegacy::default())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: ferrofin_model::entities::MediaStreamType,
            _query: &InternalItemsQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(Vec::new())
        }
        async fn queue_library_scan(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A next-up service that answers with canned keys and batch results and
    /// records the keys query it was handed.
    struct FakeNextUpService {
        keys: Vec<String>,
        batch: HashMap<String, NextUpEpisodeBatchResult>,
        keys_query: Mutex<Option<InternalItemsQuery>>,
    }
    impl FakeNextUpService {
        fn new(keys: Vec<String>, batch: HashMap<String, NextUpEpisodeBatchResult>) -> Self {
            Self {
                keys,
                batch,
                keys_query: Mutex::new(None),
            }
        }
    }
    #[async_trait]
    impl NextUpService for FakeNextUpService {
        async fn get_next_up_series_keys(
            &self,
            filter: &InternalItemsQuery,
            _date_cutoff: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<String>, ServiceError> {
            *self.keys_query.lock().unwrap() = Some(filter.clone());
            Ok(self.keys.clone())
        }
        async fn get_next_up_episodes_batch(
            &self,
            _filter: &InternalItemsQuery,
            _series_keys: &[String],
            _include_specials: bool,
            _include_watched_for_rewatching: bool,
        ) -> Result<HashMap<String, NextUpEpisodeBatchResult>, ServiceError> {
            Ok(self.batch.clone())
        }
    }

    struct FakeDtoService;
    #[async_trait]
    impl DtoService for FakeDtoService {
        async fn get_primary_image_aspect_ratio(
            &self,
            _item_id: Uuid,
        ) -> Result<Option<f64>, ServiceError> {
            Ok(None)
        }
        async fn get_base_item_dto(
            &self,
            item: &BaseItemEntity,
            _options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
        ) -> Result<BaseItemDto, ServiceError> {
            Ok(BaseItemDto {
                id: Uuid::parse_str(&item.id).unwrap_or_default(),
                ..BaseItemDto::default()
            })
        }
        async fn get_base_item_dtos(
            &self,
            items: &[BaseItemEntity],
            _options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
            _skip_visibility_check: bool,
        ) -> Result<Vec<BaseItemDto>, ServiceError> {
            Ok(items
                .iter()
                .map(|i| BaseItemDto {
                    id: Uuid::parse_str(&i.id).unwrap_or_default(),
                    ..BaseItemDto::default()
                })
                .collect())
        }
        async fn get_item_by_name_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _tagged_item_ids: Option<&[Uuid]>,
            _user: Option<&UserEntity>,
        ) -> Result<BaseItemDto, ServiceError> {
            Ok(BaseItemDto::default())
        }
    }

    struct FakeConfigManager {
        include_specials: bool,
    }
    #[async_trait]
    impl ServerConfigurationManager for FakeConfigManager {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("application_paths not used in next-up tests")
        }
        async fn configuration(
            &self,
        ) -> Result<std::sync::Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError>
        {
            let mut c = crate::configuration_manager::default_server_configuration();
            c.display_specials_within_seasons = self.include_specials;
            Ok(std::sync::Arc::new(c))
        }
        async fn update_configuration(
            &self,
            _config: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// One library folder as the user root's child, so the library-wide path
    /// has a scope.
    fn one_library() -> (Uuid, FakeLibraryManager) {
        let library = Uuid::new_v4();
        (
            library,
            FakeLibraryManager {
                items: HashMap::new(),
                root_children: vec![item(library, BaseItemKind::CollectionFolder)],
            },
        )
    }

    fn manager(
        user: UserEntity,
        library: FakeLibraryManager,
        next_up: Arc<FakeNextUpService>,
        include_specials: bool,
    ) -> FerrofinTvSeriesManager {
        FerrofinTvSeriesManager::new(
            Arc::new(FakeUserManager {
                user,
                latest_item_excludes: Vec::new(),
            }),
            Arc::new(library),
            next_up,
            Arc::new(FakeDtoService),
            Arc::new(FakeConfigManager { include_specials }),
        )
    }

    /// `n` series, each with a next-up, keyed `series-0` … `series-{n-1}`.
    fn series_with_next_up(n: usize) -> (Vec<String>, HashMap<String, NextUpEpisodeBatchResult>) {
        let mut batch = HashMap::new();
        for i in 0..n {
            batch.insert(
                format!("series-{i}"),
                batch_result(None, Some(episode(Uuid::new_v4(), 1, 1))),
            );
        }
        let keys: Vec<String> = (0..n).map(|i| format!("series-{i}")).collect();
        (keys, batch)
    }

    #[tokio::test]
    async fn library_wide_next_up_projects_picked_episodes() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let ep = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (_, library) = one_library();

        let mut batch = HashMap::new();
        batch.insert(
            "series-a".to_owned(),
            batch_result(None, Some(episode(ep, 1, 1))),
        );
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(vec!["series-a".to_owned()], batch)),
            false,
        );

        let query = NextUpQuery {
            user_id,
            enable_total_record_count: true,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, ep);
        assert_eq!(result.total_record_count, 1);
    }

    #[tokio::test]
    async fn missing_user_is_not_found() {
        let db = test_db().await;
        // Seed a user, but query a *different* id → lookup misses.
        let user = seed_user(&db, Uuid::new_v4()).await;
        let (_, library) = one_library();
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(Vec::new(), HashMap::new())),
            false,
        );
        let query = NextUpQuery {
            user_id: Uuid::new_v4(),
            ..NextUpQuery::default()
        };
        let err = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect_err("missing user");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn limit_and_start_index_paginate() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (_, library) = one_library();
        let (keys, batch) = series_with_next_up(3);
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(keys, batch)),
            false,
        );

        let query = NextUpQuery {
            user_id,
            start_index: Some(1),
            limit: Some(1),
            enable_total_record_count: true,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");

        // 3 total, skip 1, take 1.
        assert_eq!(result.total_record_count, 3);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.start_index, 1);
    }

    /// `GetResult`: `totalCount` stays `0` unless the total was asked for —
    /// never the page length. jellyfin-web's home screen sends
    /// `EnableTotalRecordCount=false` and v12 answers `TotalRecordCount: 0`.
    #[rstest]
    #[case(true, 3)]
    #[case(false, 0)]
    #[tokio::test]
    async fn total_record_count_is_zero_when_not_requested(
        #[case] enable_total_record_count: bool,
        #[case] expected_total: i32,
    ) {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (_, library) = one_library();
        let (keys, batch) = series_with_next_up(3);
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(keys, batch)),
            false,
        );

        let query = NextUpQuery {
            user_id,
            limit: Some(2),
            enable_total_record_count,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.total_record_count, expected_total);
    }

    /// The series keys are never capped by the page limit: v12 limits them
    /// only on the single-series overload, and capping here decides the page
    /// before the resumable/specials rules have dropped any pick.
    #[tokio::test]
    async fn series_keys_are_not_truncated_by_the_limit() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (library_id, library) = one_library();
        let (keys, batch) = series_with_next_up(30);
        let next_up = Arc::new(FakeNextUpService::new(keys, batch));
        let mgr = manager(user, library, Arc::clone(&next_up), false);

        let query = NextUpQuery {
            user_id,
            limit: Some(1),
            enable_total_record_count: true,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");

        let keys_query = next_up
            .keys_query
            .lock()
            .unwrap()
            .clone()
            .expect("keys queried");
        assert_eq!(
            keys_query.limit, None,
            "no `limit + 10` on the keys statement"
        );
        assert_eq!(keys_query.top_parent_ids, vec![library_id]);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total_record_count, 30, "every series was considered");
    }

    /// `DetermineNextEpisode`'s resumable rule: with `EnableResumable=false` a
    /// pick with resume progress — on any of its versions — is dropped.
    #[rstest]
    #[case(true, 2)]
    #[case(false, 1)]
    #[tokio::test]
    async fn resumable_pick_is_dropped_when_enable_resumable_is_false(
        #[case] enable_resumable: bool,
        #[case] expected_items: usize,
    ) {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (_, library) = one_library();

        let fresh = Uuid::new_v4();
        let in_progress = Uuid::new_v4();
        let mut batch = HashMap::new();
        batch.insert(
            "series-a".to_owned(),
            batch_result(None, Some(episode(fresh, 1, 1))),
        );
        let mut b = batch_result(None, Some(episode(in_progress, 1, 1)));
        b.user_data.insert(
            guid_to_db(in_progress),
            NextUpEpisodeUserData {
                played: false,
                playback_position_ticks: 12_345,
                last_played_date: None,
            },
        );
        batch.insert("series-b".to_owned(), b);
        let keys = vec!["series-a".to_owned(), "series-b".to_owned()];
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(keys, batch)),
            false,
        );

        let query = NextUpQuery {
            user_id,
            enable_resumable,
            enable_total_record_count: true,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");
        assert_eq!(result.items.len(), expected_items);
        assert_eq!(
            result.total_record_count,
            i32::try_from(expected_items).unwrap()
        );
        assert!(result.items.iter().any(|i| i.id == fresh));
        assert_eq!(
            result.items.iter().any(|i| i.id == in_progress),
            enable_resumable
        );
    }

    /// The queue is ordered by the last watched episode's play date, newest
    /// first — not by the series-key order the keys statement returned. A
    /// series with no last watched episode sorts last (`DateTime.MinValue`);
    /// one whose last watched carries no date sorts just above it
    /// (`MinValue + 1 day`).
    #[tokio::test]
    async fn picks_are_ordered_by_the_last_watched_episodes_play_date() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (_, library) = one_library();

        let older = Uuid::new_v4();
        let newer = Uuid::new_v4();
        let undated = Uuid::new_v4();
        let never_watched = Uuid::new_v4();
        let mut batch = HashMap::new();
        batch.insert(
            "series-older".to_owned(),
            batch_result(
                Some((
                    episode(Uuid::new_v4(), 1, 1),
                    Some(ts("2021-01-01T00:00:00Z")),
                )),
                Some(episode(older, 1, 2)),
            ),
        );
        batch.insert(
            "series-newer".to_owned(),
            batch_result(
                Some((
                    episode(Uuid::new_v4(), 1, 1),
                    Some(ts("2022-01-01T00:00:00Z")),
                )),
                Some(episode(newer, 1, 2)),
            ),
        );
        batch.insert(
            "series-undated".to_owned(),
            batch_result(
                Some((episode(Uuid::new_v4(), 1, 1), None)),
                Some(episode(undated, 1, 2)),
            ),
        );
        batch.insert(
            "series-never".to_owned(),
            batch_result(None, Some(episode(never_watched, 1, 1))),
        );
        // Keys deliberately in the wrong order.
        let keys = vec![
            "series-never".to_owned(),
            "series-undated".to_owned(),
            "series-older".to_owned(),
            "series-newer".to_owned(),
        ];
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(keys, batch)),
            false,
        );

        let result = mgr
            .get_next_up(
                &NextUpQuery {
                    user_id,
                    ..NextUpQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("next up");
        let ids: Vec<Uuid> = result.items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![newer, older, undated, never_watched]);
    }

    /// The parents are `GetUserRootFolder().GetChildren(user, true)` kept to
    /// `Folder`s and minus `LatestItemExcludes` — the library rows, never the
    /// library's folders at large.
    #[tokio::test]
    async fn parents_are_the_users_library_folders_minus_latest_excludes() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;

        let shows = Uuid::new_v4();
        let movies = Uuid::new_v4();
        let excluded = Uuid::new_v4();
        let playlists = Uuid::new_v4();
        let not_a_folder = Uuid::new_v4();
        let library = FakeLibraryManager {
            items: HashMap::new(),
            root_children: vec![
                item(shows, BaseItemKind::CollectionFolder),
                item(movies, BaseItemKind::CollectionFolder),
                item(excluded, BaseItemKind::CollectionFolder),
                item(playlists, BaseItemKind::PlaylistsFolder),
                item(not_a_folder, BaseItemKind::Movie),
            ],
        };
        let next_up = Arc::new(FakeNextUpService::new(Vec::new(), HashMap::new()));
        let mgr = FerrofinTvSeriesManager::new(
            Arc::new(FakeUserManager {
                user,
                latest_item_excludes: vec![excluded],
            }),
            Arc::new(library),
            Arc::clone(&next_up) as Arc<dyn NextUpService>,
            Arc::new(FakeDtoService),
            Arc::new(FakeConfigManager {
                include_specials: false,
            }),
        );

        let result = mgr
            .get_next_up(
                &NextUpQuery {
                    user_id,
                    ..NextUpQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("next up");
        assert!(result.items.is_empty());
        assert_eq!(result.total_record_count, 0);

        let keys_query = next_up
            .keys_query
            .lock()
            .unwrap()
            .clone()
            .expect("keys queried");
        assert_eq!(keys_query.top_parent_ids, vec![shows, movies, playlists]);
    }

    /// An explicit `parentId` is the scope on its own — when it exists.
    #[rstest]
    #[case(true)]
    #[case(false)]
    #[tokio::test]
    async fn an_explicit_parent_scopes_the_scan_when_it_exists(#[case] exists: bool) {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let parent = Uuid::new_v4();
        let mut items = HashMap::new();
        if exists {
            items.insert(parent, item(parent, BaseItemKind::CollectionFolder));
        }
        let library = FakeLibraryManager {
            items,
            root_children: vec![item(Uuid::new_v4(), BaseItemKind::CollectionFolder)],
        };
        let next_up = Arc::new(FakeNextUpService::new(Vec::new(), HashMap::new()));
        let mgr = manager(user, library, Arc::clone(&next_up), false);

        mgr.get_next_up(
            &NextUpQuery {
                user_id,
                parent_id: Some(parent),
                ..NextUpQuery::default()
            },
            &DtoOptions::default(),
        )
        .await
        .expect("next up");

        let keys_query = next_up.keys_query.lock().unwrap().clone();
        if exists {
            assert_eq!(
                keys_query.expect("keys queried").top_parent_ids,
                vec![parent]
            );
        } else {
            assert!(keys_query.is_none(), "a missing parent scopes to nothing");
        }
    }

    /// `GetItemById(SeriesId) is Series`: a `seriesId` naming anything else
    /// leaves the key null and the library-wide scan runs instead.
    #[tokio::test]
    async fn a_series_id_that_is_not_a_series_falls_through_to_the_library_scan() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let season = Uuid::new_v4();
        let (library_id, mut library) = one_library();
        library
            .items
            .insert(season, item(season, BaseItemKind::Season));
        let next_up = Arc::new(FakeNextUpService::new(Vec::new(), HashMap::new()));
        let mgr = manager(user, library, Arc::clone(&next_up), false);

        mgr.get_next_up(
            &NextUpQuery {
                user_id,
                series_id: Some(season),
                ..NextUpQuery::default()
            },
            &DtoOptions::default(),
        )
        .await
        .expect("next up");

        let keys_query = next_up
            .keys_query
            .lock()
            .unwrap()
            .clone()
            .expect("keys queried");
        assert_eq!(keys_query.top_parent_ids, vec![library_id]);
    }

    // ── DetermineNextEpisode: the specials merge ──

    /// A special row carrying the given `Airs*` numbers in its `Data` blob.
    fn special(
        id: Uuid,
        index: i64,
        before_season: Option<i64>,
        after_season: Option<i64>,
        before_episode: Option<i64>,
    ) -> BaseItemEntity {
        let mut blob = serde_json::Map::new();
        if let Some(n) = before_season {
            blob.insert("AirsBeforeSeasonNumber".to_owned(), n.into());
        }
        if let Some(n) = after_season {
            blob.insert("AirsAfterSeasonNumber".to_owned(), n.into());
        }
        if let Some(n) = before_episode {
            blob.insert("AirsBeforeEpisodeNumber".to_owned(), n.into());
        }
        BaseItemEntity {
            data: Some(serde_json::Value::Object(blob).to_string()),
            ..episode(id, 0, index)
        }
    }

    /// Adds a special to a batch result, with its played flag.
    fn with_special(
        mut result: NextUpEpisodeBatchResult,
        row: BaseItemEntity,
        played: bool,
    ) -> NextUpEpisodeBatchResult {
        result.user_data.insert(
            row.id.clone(),
            NextUpEpisodeUserData {
                played,
                ..NextUpEpisodeUserData::default()
            },
        );
        result.specials.push(row);
        result
    }

    /// Watched through S1E5, next up S2E1, and a special that airs after
    /// season 1: in aired order the special sits between them, so it is the
    /// pick — unless it was already played, or airs at no known position.
    #[rstest]
    #[case::airs_after_season_one(Some(1), None, false, true)]
    #[case::airs_before_season_two(None, Some(2), false, true)]
    #[case::played_special_is_skipped(Some(1), None, true, false)]
    #[case::unpositioned_special_is_ignored(None, None, false, false)]
    fn specials_merge_picks_a_special_airing_between_last_watched_and_next(
        #[case] airs_after: Option<i64>,
        #[case] airs_before: Option<i64>,
        #[case] played: bool,
        #[case] expect_special: bool,
    ) {
        let next = Uuid::new_v4();
        let sp = Uuid::new_v4();
        let result = with_special(
            batch_result(
                Some((episode(Uuid::new_v4(), 1, 5), None)),
                Some(episode(next, 2, 1)),
            ),
            special(sp, 1, airs_before, airs_after, None),
            played,
        );
        let pick = determine_next_episode(&result, true, true, false).expect("a pick");
        let expected = if expect_special { sp } else { next };
        assert_eq!(pick.id, guid_to_db(expected));
    }

    /// With `DisplaySpecialsWithinSeasons` off the specials are not consulted
    /// at all, even when the service fetched some.
    #[test]
    fn specials_are_ignored_unless_displayed_within_seasons() {
        let next = Uuid::new_v4();
        let result = with_special(
            batch_result(
                Some((episode(Uuid::new_v4(), 1, 5), None)),
                Some(episode(next, 2, 1)),
            ),
            special(Uuid::new_v4(), 1, Some(2), None, None),
            false,
        );
        let pick = determine_next_episode(&result, false, true, false).expect("a pick");
        assert_eq!(pick.id, guid_to_db(next));
    }

    /// A special that airs before the very episode the user stopped at is
    /// behind the last watched position and cannot be the pick.
    #[test]
    fn specials_behind_the_last_watched_position_are_skipped() {
        let next = Uuid::new_v4();
        let result = with_special(
            batch_result(
                Some((episode(Uuid::new_v4(), 1, 5), None)),
                Some(episode(next, 1, 6)),
            ),
            special(Uuid::new_v4(), 1, Some(1), None, Some(3)),
            false,
        );
        let pick = determine_next_episode(&result, true, true, false).expect("a pick");
        assert_eq!(pick.id, guid_to_db(next));
    }

    /// The resumable rule applies after the merge — to whatever the pick is.
    #[test]
    fn a_resumable_special_pick_is_dropped_too() {
        let sp = Uuid::new_v4();
        let mut result = with_special(
            batch_result(
                Some((episode(Uuid::new_v4(), 1, 5), None)),
                Some(episode(Uuid::new_v4(), 2, 1)),
            ),
            special(sp, 1, Some(2), None, None),
            false,
        );
        result
            .user_data
            .get_mut(&guid_to_db(sp))
            .expect("special facts")
            .playback_position_ticks = 1;
        assert!(determine_next_episode(&result, true, false, false).is_none());
        assert_eq!(
            determine_next_episode(&result, true, true, false).map(|e| e.id),
            Some(guid_to_db(sp))
        );
    }

    /// `DetermineNextEpisodeForRewatching` passes `includeResumable: false`
    /// unconditionally: a rewatch pick with resume progress is dropped even
    /// when the request allows resumable fresh picks.
    #[tokio::test]
    async fn a_resumable_rewatch_pick_is_dropped_regardless_of_enable_resumable() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        let (_, library) = one_library();

        let fresh = Uuid::new_v4();
        let rewatch = Uuid::new_v4();
        let mut result = batch_result(None, Some(episode(fresh, 3, 1)));
        result.last_watched_for_rewatching = Some(episode(Uuid::new_v4(), 1, 1));
        result.next_played_for_rewatching = Some(episode(rewatch, 1, 2));
        result.user_data.insert(
            guid_to_db(rewatch),
            NextUpEpisodeUserData {
                played: true,
                playback_position_ticks: 9,
                last_played_date: None,
            },
        );
        let mut batch = HashMap::new();
        batch.insert("series-a".to_owned(), result);
        let mgr = manager(
            user,
            library,
            Arc::new(FakeNextUpService::new(vec!["series-a".to_owned()], batch)),
            false,
        );

        let out = mgr
            .get_next_up(
                &NextUpQuery {
                    user_id,
                    enable_resumable: true,
                    enable_rewatching: true,
                    ..NextUpQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("next up");
        let ids: Vec<Uuid> = out.items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![fresh]);
    }

    /// The comparer is not a total order (an unnumbered episode compares
    /// equal to a positioned special while ordering strictly against its
    /// numbered siblings); the merge must sort such a list without panicking
    /// and keep it stable.
    #[test]
    fn specials_merge_survives_an_intransitive_comparison() {
        let next = Uuid::new_v4();
        let mut result = batch_result(
            Some((
                BaseItemEntity {
                    index_number: None,
                    ..episode(Uuid::new_v4(), 1, 0)
                },
                None,
            )),
            Some(episode(next, 1, 6)),
        );
        for i in 0..40 {
            result = with_special(
                result,
                special(Uuid::new_v4(), i, Some(1), None, Some(2)),
                false,
            );
        }
        // Stable: the specials keep their order, the unnumbered last watched
        // row stays behind them (it compares equal to each) and the numbered
        // next row sorts last, so the pick is that row — an unstable sort
        // could place the last watched row among the specials and pick one.
        assert_eq!(
            determine_next_episode(&result, true, true, false).map(|e| e.id),
            Some(guid_to_db(next))
        );
    }

    /// Rewatching decides the `*ForRewatching` pair and keeps played rows.
    #[test]
    fn rewatching_pick_comes_from_the_rewatching_pair() {
        let next_played = Uuid::new_v4();
        let mut result = batch_result(None, Some(episode(Uuid::new_v4(), 3, 1)));
        result.last_watched_for_rewatching = Some(episode(Uuid::new_v4(), 1, 1));
        result.next_played_for_rewatching = Some(episode(next_played, 1, 2));
        result.user_data.insert(
            guid_to_db(next_played),
            NextUpEpisodeUserData {
                played: true,
                ..NextUpEpisodeUserData::default()
            },
        );
        let pick = determine_next_episode(&result, false, false, true).expect("a pick");
        assert_eq!(pick.id, guid_to_db(next_played));
    }

    // ── AiredEpisodeOrderComparer, transliterated from upstream's
    //    `AiredEpisodeOrderComparerTests.EpisodeTestData` (episode rows only;
    //    the `Movie` rows cannot occur here). ──

    /// A comparer operand: `(season, episode, before_season, after_season,
    /// before_episode, premiere)`.
    fn order(
        season: Option<i64>,
        episode: Option<i64>,
        before_season: Option<i64>,
        after_season: Option<i64>,
        before_episode: Option<i64>,
        premiere: Option<&str>,
    ) -> AiredOrder {
        AiredOrder {
            season,
            episode,
            premiere: premiere.map(ts),
            airs_before_season: before_season,
            airs_after_season: after_season,
            airs_before_episode: before_episode,
        }
    }

    #[rstest]
    #[case(
        order(None, None, None, None, None, None),
        order(None, None, None, None, None, None),
        Ordering::Equal
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, None),
        order(Some(1), Some(1), None, None, None, None),
        Ordering::Equal
    )]
    #[case(
        order(Some(1), Some(2), None, None, None, None),
        order(Some(1), Some(1), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(2), Some(1), None, None, None, None),
        order(Some(1), Some(1), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(0), Some(1), None, None, None, None),
        order(Some(0), Some(1), None, None, None, None),
        Ordering::Equal
    )]
    #[case(
        order(Some(0), Some(2), None, None, None, None),
        order(Some(0), Some(1), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, None),
        order(Some(0), Some(1), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, None),
        order(Some(0), Some(2), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), Some(2), None, None, None, None),
        order(Some(0), Some(1), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(0), Some(1), None, Some(1), None, None),
        order(Some(1), Some(1), None, None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(3), Some(1), None, None, None, None),
        order(Some(0), Some(1), None, Some(1), None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(3), Some(1), None, None, None, None),
        order(Some(0), Some(1), None, Some(1), Some(2), None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, None),
        order(Some(0), Some(1), Some(1), None, None, None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), Some(2), None, None, None, None),
        order(Some(0), Some(1), Some(1), None, Some(2), None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), None, None, None, None, None),
        order(Some(0), Some(1), Some(1), None, Some(2), None),
        Ordering::Equal
    )]
    #[case(
        order(Some(1), Some(3), None, None, None, None),
        order(Some(0), Some(1), Some(1), None, Some(2), None),
        Ordering::Greater
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, Some("2021-09-12T00:00:00Z")),
        order(Some(1), Some(1), None, None, None, Some("2021-09-12T00:00:00Z")),
        Ordering::Equal
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, Some("2021-09-11T00:00:00Z")),
        order(Some(1), Some(1), None, None, None, Some("2021-09-12T00:00:00Z")),
        Ordering::Less
    )]
    #[case(
        order(Some(1), Some(1), None, None, None, Some("2021-09-12T00:00:00Z")),
        order(Some(1), Some(1), None, None, None, Some("2021-09-11T00:00:00Z")),
        Ordering::Greater
    )]
    fn aired_episode_order_compare(
        #[case] x: AiredOrder,
        #[case] y: AiredOrder,
        #[case] expected: Ordering,
    ) {
        assert_eq!(x.compare(&y), expected);
        assert_eq!(y.compare(&x), expected.reverse());
    }

    /// The `Airs*` numbers are read out of a special's `Data` blob, and only a
    /// special's.
    #[test]
    fn aired_order_reads_airs_numbers_from_the_data_blob_of_specials_only() {
        let sp = special(Uuid::new_v4(), 1, Some(1), None, Some(2));
        let o = AiredOrder::of(&sp);
        assert_eq!(o.airs_before_season, Some(1));
        assert_eq!(o.airs_before_episode, Some(2));
        assert_eq!(o.airs_after_season, None);

        let regular = BaseItemEntity {
            data: Some(r#"{"AirsAfterSeasonNumber":7}"#.to_owned()),
            ..episode(Uuid::new_v4(), 1, 1)
        };
        assert_eq!(AiredOrder::of(&regular).airs_after_season, None);
    }
}
