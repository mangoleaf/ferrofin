//! [`FerrofinNextUpService`] — the concrete [`NextUpService`] over `ferrofin-db`.
//!
//! Port of `NextUpService` (v12 `Jellyfin.Server.Implementations/Item/NextUpService.cs`).
//! Computes, per TV series, the "next up" episode for a user given their watch
//! history. The C# service returns deserialized domain `BaseItem`s via
//! `IItemQueryHelpers.DeserializeBaseItem`; the trait here returns raw
//! [`BaseItemEntity`] rows (deserialization is a DTO-layer concern), so no
//! query-helper sibling is taken. The result feeds `TvSeriesManager`, which does
//! the presentation-layer assembly.
//!
//! Access-filtering (`ApplyAccessFiltering`) and navigation-loading
//! (`ApplyNavigations`) in C# widen/narrow the row set through the library
//! manager's parental controls; those are not part of this repository seam, so
//! the queries here use the item type + series key + played/virtual predicates
//! that define the next-up algorithm and leave the parental widening to the
//! caller. Episodes are ordered by `(ParentIndexNumber, IndexNumber)` — season,
//! then episode — exactly as C#.
//!
//! # Round-trip budget
//!
//! Every query here is batched over *all* requested series keys — nothing runs
//! per series. One next-up request costs:
//!
//! * 1 query mapping the scope's collection folders to their physical
//!   folders, then 1 for the eligible series keys ([`get_next_up_series_keys`]);
//! * 1 query for the projection of every episode of those series joined to the
//!   user's `UserData` — `(id, season, episode, virtual, version group,
//!   played, resume position, last played)` — from which the played, unplayed
//!   and played-non-virtual pools, the rewatching "last played by date" pick
//!   and the per-episode user-data facts are all derived;
//! * 1 query for the specials' full rows, only when specials were requested;
//! * 1 query for the full rows of the (few) episodes the batch result actually
//!   returns.
//!
//! So 4 round-trips in the default configuration and 5 with specials on. The
//! picked-episode positions come from the projection, so no full row has to be
//! loaded just to read its season/episode, and each full row is *moved* into the
//! batch result (cloned only when two fields of the same series name it).
//!
//! [`get_next_up_series_keys`]: NextUpService::get_next_up_series_keys

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{
    NextUpEpisodeBatchResult, NextUpEpisodeUserData, NextUpService,
};

use crate::db_error::db_err;
use crate::item_repository::physical_folders_by_view;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::PLACEHOLDER_ID;

/// The concrete next-up service.
#[derive(Clone)]
pub struct FerrofinNextUpService {
    db: Database,
}

impl std::fmt::Debug for FerrofinNextUpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinNextUpService")
            .finish_non_exhaustive()
    }
}

impl FerrofinNextUpService {
    /// Creates a next-up service over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// The stored `Type` name for episodes, or an error if the lookup lacks it.
    fn episode_type() -> Result<&'static str, ServiceError> {
        stored_type_name(BaseItemKind::Episode)
            .ok_or_else(|| ServiceError::backend("no stored type name for Episode"))
    }

    /// Fetches full episode rows for the given ids, keyed by id.
    async fn episodes_by_ids(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, BaseItemEntity>, ServiceError> {
        let mut map = HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut sql = String::from(r#"SELECT * FROM "BaseItems" WHERE "Id" IN ("#);
        push_key_placeholders(&mut sql, ids.len());
        sql.push(')');
        let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql);
        for id in ids {
            query = query.bind(guid_to_db(*id));
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        for row in rows {
            if let Ok(id) = Uuid::parse_str(&row.id) {
                map.insert(id, row);
            }
        }
        Ok(map)
    }

    /// The `TopParentId` values a set of scope parents stands for — C#
    /// `LibraryManager.GetNextUpSeriesKeys`'s `SetTopParentIdsOrAncestors`
    /// step, per parent (`GetTopParentIdsForQuery`): a collection folder
    /// contributes its `PhysicalFolderIds`, anything else — a Live TV or
    /// playlists view, a physical folder, or any id on a Ferrofin-written
    /// database where the view IS the top parent — contributes itself.
    ///
    /// The manager hands over the user's library folders (3–7 ids), and this
    /// list must stay that small: the keys statement's `TopParentId IN (…)`
    /// is evaluated per candidate row, and scoping to every folder in the
    /// library (seasons, albums, artists — 1,975 on the bench fixture)
    /// against the retired `CROSS JOIN` shape, which walked it once per
    /// `UserData` row, is what made the statement 10 M index probes and 1.4 s.
    async fn top_parent_ids(&self, parents: &[Uuid]) -> Result<Vec<Uuid>, ServiceError> {
        let by_view = physical_folders_by_view(&self.db, parents).await?;
        let mut out = Vec::with_capacity(parents.len());
        for id in parents {
            match by_view.get(id) {
                Some(folders) => out.extend(folders.iter().copied()),
                None => out.push(*id),
            }
        }
        Ok(out)
    }
}

/// A lightweight `(id, season, episode)` projection used to pick the next/last
/// episode without loading full rows (mirrors the C# anonymous projection).
///
/// `is_real` mirrors the SQL `IsVirtualItem = 0` predicate: only *real* (on-disk)
/// episodes are eligible as a next-up pick, while the last-watched position is
/// taken over played episodes whether or not they are virtual.
#[derive(Clone, Copy)]
struct EpisodePos {
    id: Uuid,
    season: Option<i64>,
    episode: Option<i64>,
    is_real: bool,
    /// The user's most recent play date on this row (rewatching mode's
    /// "last played by date" reads it).
    last_played: Option<DateTime<Utc>>,
}

impl EpisodePos {
    /// The `(season, episode)` position, or `None` when either number is missing
    /// — the same nullability rule the C# uses when comparing positions, so a
    /// last-watched episode with no numbers searches from the start of the series.
    fn position(self) -> Option<(i64, i64)> {
        Some((self.season?, self.episode?))
    }
}

/// A row of the episode projection: `(series key, id, season, episode, is
/// virtual, primary version id, owner id, extra type, played, resume
/// position, last played)`. The three user-data columns come off a `LEFT
/// JOIN`, so they are `NULL` for an episode the user has no data on.
type ProjectionRow = (
    String,
    String,
    Option<i64>,
    Option<i64>,
    bool,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<bool>,
    Option<i64>,
    Option<DateTime<Utc>>,
);

/// One episode of the projection after the per-id merge (a Jellyfin database
/// holds one `UserData` row per user-data *key* — id, provider id, series
/// position — so the join yields the same episode two or three times).
struct ProjectedEpisode {
    key: String,
    pos: EpisodePos,
    /// The id every version of this episode shares: its `PrimaryVersionId`
    /// when it is an alternate, else its own id (C# `GetAllVersions` groups
    /// a primary with the alternates linked onto it).
    version_group: Uuid,
    /// Whether the row may be picked at all — C# `ApplyAccessFiltering`'s
    /// `PrimaryVersionId == null && (OwnerId == null || ExtraType != null)`:
    /// alternate versions and owned non-extras are never last watched, next
    /// up or a special, though their user data still counts for the group.
    is_candidate: bool,
    user_data: NextUpEpisodeUserData,
}

/// The episode pools of one series, partitioned out of the single projection
/// query. `played` keeps virtual rows (the last-watched position may sit on a
/// virtual episode); `unplayed` holds only real, unplayed episodes.
#[derive(Default)]
struct SeriesEpisodes {
    /// Played, non-special episodes — virtual rows included.
    played: Vec<EpisodePos>,
    /// Unplayed, non-special, non-virtual episodes — the next-up candidates.
    unplayed: Vec<EpisodePos>,
}

#[async_trait]
impl NextUpService for FerrofinNextUpService {
    async fn get_next_up_series_keys(
        &self,
        filter: &InternalItemsQuery,
        date_cutoff: DateTime<Utc>,
    ) -> Result<Vec<String>, ServiceError> {
        let Some(user) = filter.user.as_ref() else {
            return Err(ServiceError::invalid_input("next-up requires a user"));
        };
        if filter.top_parent_ids.is_empty() {
            return Ok(Vec::new());
        }
        let episode_type = Self::episode_type()?;
        let top_parents = self.top_parent_ids(&filter.top_parent_ids).await?;

        // Series (by presentation key) whose most-recently-played episode within
        // the requested libraries is at/after the cutoff, newest first.
        let sql = next_up_series_keys_sql(top_parents.len(), filter.limit.is_some());
        let mut query = sqlx::query_scalar::<_, String>(&sql)
            .bind(&user.id)
            .bind(episode_type)
            .bind(PLACEHOLDER_ID);
        for id in &top_parents {
            query = query.bind(guid_to_db(*id));
        }
        query = query.bind(datetime_to_db(date_cutoff));
        if let Some(limit) = filter.limit {
            // C# `Take(n)` with `n <= 0` yields nothing; SQLite reads a
            // negative `LIMIT` as "no limit".
            query = query.bind(i64::from(limit.max(0)));
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    async fn get_next_up_episodes_batch(
        &self,
        filter: &InternalItemsQuery,
        series_keys: &[String],
        include_specials: bool,
        include_watched_for_rewatching: bool,
    ) -> Result<HashMap<String, NextUpEpisodeBatchResult>, ServiceError> {
        let Some(user) = filter.user.as_ref() else {
            return Err(ServiceError::invalid_input("next-up requires a user"));
        };
        if series_keys.is_empty() {
            return Ok(HashMap::new());
        }
        let episode_type = Self::episode_type()?;

        // ── One projection query feeds every position-based decision below ──
        let projected = self
            .episode_projection(episode_type, &user.id, series_keys)
            .await?;

        // The user-data facts of every projected episode, with the resume
        // position and play date merged across each version group — the
        // `GetAllVersions` walk `DetermineNextEpisode` and
        // `GetMostRecentlyPlayedVersion` make, done once here.
        let user_data = merge_version_groups(&projected);

        let by_key = partition_pools(&projected);

        // Last watched: highest season/episode among played, non-special rows.
        let last_watched_by_key = pick_per_series(&by_key, highest_position);

        // ── Last watched by play date (rewatching mode) ──
        let last_watched_by_date = if include_watched_for_rewatching {
            pick_per_series(&by_key, most_recently_played)
        } else {
            HashMap::new()
        };

        // ── Specials (season 0), non-virtual ──
        let mut specials_by_key = if include_specials {
            self.fetch_specials(episode_type, series_keys).await?
        } else {
            HashMap::new()
        };

        // ── Next up: first unplayed episode after the last-watched position ──
        let mut next_up_by_key: HashMap<&str, Uuid> = HashMap::new();
        for (key, episodes) in &by_key {
            let after = last_watched_by_key.get(key).and_then(|pos| pos.position());
            if let Some(next) = first_after(&episodes.unplayed, after) {
                next_up_by_key.insert(key, next);
            }
        }

        // ── Next played (rewatching): first played episode after last-by-date ──
        let mut next_played_by_key: HashMap<&str, Uuid> = HashMap::new();
        if include_watched_for_rewatching {
            for (key, last) in &last_watched_by_date {
                let Some(episodes) = by_key.get(key) else {
                    continue;
                };
                // The rewatching candidates are the *real* played episodes.
                let candidates: Vec<EpisodePos> = episodes
                    .played
                    .iter()
                    .copied()
                    .filter(|pos| pos.is_real)
                    .collect();
                if let Some(next) = first_after(&candidates, last.position()) {
                    next_played_by_key.insert(key, next);
                }
            }
        }

        // ── One fetch of the full rows the batch result actually returns ──
        let mut refs: HashMap<Uuid, usize> = HashMap::new();
        for id in last_watched_by_key
            .values()
            .map(|pos| pos.id)
            .chain(next_up_by_key.values().copied())
            .chain(last_watched_by_date.values().map(|pos| pos.id))
            .chain(next_played_by_key.values().copied())
        {
            *refs.entry(id).or_insert(0) += 1;
        }
        let wanted: Vec<Uuid> = refs.keys().copied().collect();
        let mut rows = self.episodes_by_ids(&wanted).await?;

        // ── Assemble a batch result per series key ──
        let mut result: HashMap<String, NextUpEpisodeBatchResult> = HashMap::new();
        for key in series_keys {
            // Duplicate keys would otherwise re-take rows already moved out.
            if result.contains_key(key) {
                continue;
            }
            let mut batch = NextUpEpisodeBatchResult::default();
            if let Some(pos) = last_watched_by_key.get(key.as_str()) {
                batch.last_watched = take_row(&mut rows, &mut refs, pos.id);
            }
            if let Some(id) = next_up_by_key.get(key.as_str()) {
                batch.next_up = take_row(&mut rows, &mut refs, *id);
            }
            if include_specials {
                batch.specials = specials_by_key.remove(key).unwrap_or_default();
            }
            if include_watched_for_rewatching {
                if let Some(pos) = last_watched_by_date.get(key.as_str()) {
                    batch.last_watched_for_rewatching = take_row(&mut rows, &mut refs, pos.id);
                }
                if let Some(id) = next_played_by_key.get(key.as_str()) {
                    batch.next_played_for_rewatching = take_row(&mut rows, &mut refs, *id);
                }
            }
            attach_user_data(&mut batch, &user_data);
            result.insert(key.clone(), batch);
        }
        Ok(result)
    }
}

impl FerrofinNextUpService {
    /// The non-virtual special (season 0) episode rows per series key — the
    /// primary, non-owned rows only, as `ApplyAccessFiltering` leaves them.
    async fn fetch_specials(
        &self,
        episode_type: &str,
        series_keys: &[String],
    ) -> Result<HashMap<String, Vec<BaseItemEntity>>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT * FROM "BaseItems"
               WHERE "Type" = ? AND "ParentIndexNumber" = 0 AND "IsVirtualItem" = 0
                 AND "PrimaryVersionId" IS NULL
                 AND ("OwnerId" IS NULL OR "OwnerId" = ?
                      OR "ExtraType" IS NOT NULL)
                 AND "SeriesPresentationUniqueKey" IN ("#,
        );
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push(')');
        let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql)
            .bind(episode_type)
            .bind(guid_to_db(Uuid::nil()));
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        let mut out: HashMap<String, Vec<BaseItemEntity>> = HashMap::new();
        for row in rows {
            if let Some(key) = row.series_presentation_unique_key.clone() {
                out.entry(key).or_default().push(row);
            }
        }
        Ok(out)
    }

    /// Fetches the projection of every episode (specials included) of the
    /// given series, joined to the user's `UserData`, merged per episode id.
    ///
    /// This is deliberately **one** query covering every pool the batch
    /// algorithm needs (played incl. virtual, unplayed real, played real, the
    /// specials' played flags) *and* the user-data facts the manager's
    /// `DetermineNextEpisode` reads per pick (resume position, play date). The
    /// join is `LEFT`, so an episode with no user data is still a candidate.
    async fn episode_projection(
        &self,
        episode_type: &str,
        user_id: &str,
        series_keys: &[String],
    ) -> Result<Vec<ProjectedEpisode>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT bi."SeriesPresentationUniqueKey", bi."Id",
                      bi."ParentIndexNumber", bi."IndexNumber", bi."IsVirtualItem",
                      bi."PrimaryVersionId", bi."OwnerId", bi."ExtraType",
                      ud."Played", ud."PlaybackPositionTicks", ud."LastPlayedDate"
               FROM "BaseItems" bi
               LEFT JOIN "UserData" ud ON ud."ItemId" = bi."Id" AND ud."UserId" = ?
               WHERE bi."Type" = ? AND bi."SeriesPresentationUniqueKey" IN ("#,
        );
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push(')');

        let mut query = sqlx::query_as::<_, ProjectionRow>(&sql)
            .bind(user_id)
            .bind(episode_type);
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;

        let mut out: Vec<ProjectedEpisode> = Vec::new();
        let mut slot_of: HashMap<Uuid, usize> = HashMap::new();
        for (
            key,
            id,
            season,
            episode,
            is_virtual,
            primary,
            owner,
            extra_type,
            played,
            ticks,
            last_played,
        ) in rows
        {
            let Ok(id) = Uuid::parse_str(&id) else {
                continue;
            };
            let user_data = NextUpEpisodeUserData {
                played: played.unwrap_or(false),
                playback_position_ticks: ticks.unwrap_or(0),
                last_played_date: last_played,
            };
            if let Some(&slot) = slot_of.get(&id) {
                // Another user-data key of the same episode: merge the facts.
                let ep = &mut out[slot];
                ep.user_data = merge_user_data(ep.user_data, user_data);
                ep.pos.last_played = ep.user_data.last_played_date;
                continue;
            }
            slot_of.insert(id, out.len());
            let primary = primary
                .as_deref()
                .and_then(|p| Uuid::parse_str(p).ok())
                .filter(|p| !p.is_nil());
            let is_candidate =
                primary.is_none() && (is_unowned(owner.as_deref()) || extra_type.is_some());
            let version_group = primary.unwrap_or(id);
            out.push(ProjectedEpisode {
                key,
                pos: EpisodePos {
                    id,
                    season,
                    episode,
                    is_real: !is_virtual,
                    last_played,
                },
                version_group,
                is_candidate,
                user_data,
            });
        }
        Ok(out)
    }
}

/// Whether an `OwnerId` means "no owner". C# treats `Guid.Empty` as unowned,
/// and a real Jellyfin database stores the ZERO GUID on virtually every row
/// while Ferrofin's writer leaves the column NULL — the same rule as
/// `translate_query`'s `NO_OWNER`, without which an adopted library has no
/// next-up candidates at all.
fn is_unowned(owner: Option<&str>) -> bool {
    owner.is_none_or(|o| Uuid::parse_str(o).is_ok_and(|id| id.is_nil()))
}

/// Partitions the non-special projected rows into each series' played /
/// unplayed pools. C# `e.ParentIndexNumber != 0` under C# null semantics: an
/// unnumbered season is not a special.
fn partition_pools(projected: &[ProjectedEpisode]) -> HashMap<&str, SeriesEpisodes> {
    let mut by_key: HashMap<&str, SeriesEpisodes> = HashMap::new();
    for ep in projected {
        if ep.pos.season == Some(0) || !ep.is_candidate {
            continue;
        }
        let bucket = by_key.entry(ep.key.as_str()).or_default();
        if ep.user_data.played {
            bucket.played.push(ep.pos);
        } else if ep.pos.is_real {
            bucket.unplayed.push(ep.pos);
        }
    }
    by_key
}

/// Applies `pick` to each series' played pool, keeping the series that have
/// an answer.
fn pick_per_series<'a>(
    by_key: &HashMap<&'a str, SeriesEpisodes>,
    pick: fn(&[EpisodePos]) -> Option<EpisodePos>,
) -> HashMap<&'a str, EpisodePos> {
    by_key
        .iter()
        .filter_map(|(key, episodes)| pick(&episodes.played).map(|pos| (*key, pos)))
        .collect()
}

/// Merges two user-data readings of the same episode: played if any key says
/// so, the furthest resume position, the most recent play date.
fn merge_user_data(a: NextUpEpisodeUserData, b: NextUpEpisodeUserData) -> NextUpEpisodeUserData {
    NextUpEpisodeUserData {
        played: a.played || b.played,
        playback_position_ticks: a.playback_position_ticks.max(b.playback_position_ticks),
        last_played_date: a.last_played_date.max(b.last_played_date),
    }
}

/// The per-episode user-data facts with the resume position and play date
/// taken over each episode's whole version group — C# `GetAllVersions()`:
/// `DetermineNextEpisode` drops a pick whose resume progress lives on an
/// alternate version, and `GetMostRecentlyPlayedVersion` dates the last
/// watched episode by whichever version was played last. `played` stays the
/// episode's own row, as `GetUserData(user, episode)` reads it.
fn merge_version_groups(projected: &[ProjectedEpisode]) -> HashMap<String, NextUpEpisodeUserData> {
    let mut group_max: HashMap<Uuid, (i64, Option<DateTime<Utc>>)> = HashMap::new();
    for ep in projected {
        let entry = group_max.entry(ep.version_group).or_insert((0, None));
        entry.0 = entry.0.max(ep.user_data.playback_position_ticks);
        entry.1 = entry.1.max(ep.user_data.last_played_date);
    }
    projected
        .iter()
        .map(|ep| {
            let (ticks, date) = group_max
                .get(&ep.version_group)
                .copied()
                .unwrap_or((0, None));
            (
                guid_to_db(ep.pos.id),
                NextUpEpisodeUserData {
                    played: ep.user_data.played,
                    playback_position_ticks: ticks,
                    last_played_date: date,
                },
            )
        })
        .collect()
}

/// Copies the user-data facts of every row `batch` carries into
/// [`NextUpEpisodeBatchResult::user_data`].
fn attach_user_data(
    batch: &mut NextUpEpisodeBatchResult,
    user_data: &HashMap<String, NextUpEpisodeUserData>,
) {
    let ids = batch
        .last_watched
        .iter()
        .chain(batch.next_up.iter())
        .chain(batch.specials.iter())
        .chain(batch.last_watched_for_rewatching.iter())
        .chain(batch.next_played_for_rewatching.iter())
        .map(|row| row.id.clone());
    for id in ids {
        if let Some(facts) = user_data.get(&id) {
            batch.user_data.insert(id, *facts);
        }
    }
}

/// Moves the row for `id` out of `rows`, cloning only while another batch field
/// still references it (the same episode can be both the last watched and the
/// last watched *by date*).
fn take_row(
    rows: &mut HashMap<Uuid, BaseItemEntity>,
    refs: &mut HashMap<Uuid, usize>,
    id: Uuid,
) -> Option<BaseItemEntity> {
    match refs.get_mut(&id) {
        Some(remaining) if *remaining > 1 => {
            *remaining -= 1;
            rows.get(&id).cloned()
        }
        _ => {
            refs.remove(&id);
            rows.remove(&id)
        }
    }
}

/// Appends `?, ?, …` of length `n` (at least one, `NULL` when empty).
fn push_key_placeholders(sql: &mut String, n: usize) {
    if n == 0 {
        sql.push_str("NULL");
        return;
    }
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
}

/// The `(season, episode)` sort key of an episode row, missing numbers sorting
/// lowest (`i64::MIN`), matching the C# nullable ordering.
fn sort_key(season: Option<i64>, episode: Option<i64>) -> (i64, i64) {
    (season.unwrap_or(i64::MIN), episode.unwrap_or(i64::MIN))
}

/// The highest-numbered episode in a group (last watched).
fn highest_position(positions: &[EpisodePos]) -> Option<EpisodePos> {
    positions
        .iter()
        .max_by_key(|p| sort_key(p.season, p.episode))
        .copied()
}

/// The most recently played episode in a group (rewatching's "last watched by
/// date"): C# `OrderByDescending(LastPlayedDate).DistinctBy(key)`, so a dated
/// row beats an undated one and the first seen wins an exact tie.
fn most_recently_played(positions: &[EpisodePos]) -> Option<EpisodePos> {
    let mut winner: Option<EpisodePos> = None;
    for pos in positions {
        match winner {
            Some(w) if pos.last_played <= w.last_played => {}
            _ => winner = Some(*pos),
        }
    }
    winner
}

/// The id of the first episode strictly after `after` (or the very first when
/// `after` is `None`), in season/episode order.
fn first_after(positions: &[EpisodePos], after: Option<(i64, i64)>) -> Option<Uuid> {
    let mut sorted: Vec<&EpisodePos> = positions.iter().collect();
    sorted.sort_by_key(|p| sort_key(p.season, p.episode));
    sorted
        .into_iter()
        .find(|p| match after {
            None => true,
            Some(pos) => sort_key(p.season, p.episode) > pos,
        })
        .map(|p| p.id)
}

/// The series-keys aggregate for next-up, with `parents` bound `TopParentId`
/// placeholders and, when `limited`, a trailing bound `LIMIT`.
///
/// Bind order: user id, episode type, placeholder id, the parents, the cutoff,
/// then the limit.
///
/// The v12 shape (`NextUpService.GetNextUpSeriesKeys`): `BaseItems` filtered
/// on `(Type, TopParentId IN …)` joined to the user's `UserData`, grouped by
/// series key, `HAVING MAX(LastPlayedDate) >= cutoff`, newest first. No join
/// pin: with the scope being the user's few library folders the planner
/// walks the episodes through a `Type`-led index (it takes the
/// `(Type, SeriesPresentationUniqueKey, …)` one, whose order serves the
/// `GROUP BY`) and seeks `UserData` by `(UserId, ItemId)` on its covering
/// index — 6 ms for 7,490 episodes on the adopted bench database, and
/// single-digit milliseconds in either join order (pinned by
/// `tests/next_up_query_plan.rs`). The `CROSS JOIN` this used to carry was tuned for a
/// user with one `UserData` row against a scope of every folder in the
/// library, and iterated the `IN` list once per `UserData` row — 5,044 ×
/// 1,975 index probes and 1.4 s on the bench fixture.
#[must_use]
pub fn next_up_series_keys_sql(parents: usize, limited: bool) -> String {
    let mut sql = String::from(
        r#"SELECT bi."SeriesPresentationUniqueKey" AS key,
                  MAX(ud."LastPlayedDate") AS last_played
           FROM "BaseItems" bi
           JOIN "UserData" ud ON ud."ItemId" = bi."Id" AND ud."UserId" = ?
           WHERE bi."Type" = ?
             AND ud."ItemId" <> ?
             AND bi."SeriesPresentationUniqueKey" IS NOT NULL
             AND bi."TopParentId" IN ("#,
    );
    push_key_placeholders(&mut sql, parents);
    sql.push_str(
        r#") GROUP BY bi."SeriesPresentationUniqueKey"
           HAVING last_played IS NOT NULL AND last_played >= ?
           ORDER BY last_played DESC"#,
    );
    if limited {
        sql.push_str(" LIMIT ?");
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::FerrofinNextUpService;
    use crate::test_support::{
        clear_index_number, corrupt_last_played_date, seed_episode, seed_item_with_data, seed_user,
        seed_user_data, test_db,
    };
    use chrono::{DateTime, Utc};
    use ferrofin_db::Database;
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::options::InternalItemsQuery;
    use ferrofin_traits::persistence::NextUpService;
    use uuid::Uuid;

    /// Parses an RFC3339 timestamp into a UTC datetime for seeding user-data.
    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid timestamp")
    }

    fn query_for(user: UserEntity, top_parent: Uuid) -> InternalItemsQuery {
        InternalItemsQuery {
            user: Some(user),
            top_parent_ids: vec![top_parent],
            ..Default::default()
        }
    }

    /// Runs one fixture statement with string binds (SQLite's column affinity
    /// turns a numeric string into the integer the column holds).
    async fn exec(db: &Database, sql: &str, binds: &[&str]) {
        let mut query = sqlx::query(sql);
        for bind in binds {
            query = query.bind((*bind).to_owned());
        }
        query.execute(db.writer()).await.expect("fixture statement");
    }

    /// Links `alternate` onto `primary` as an alternate version (what
    /// `MergeVersions` writes).
    async fn set_primary_version(db: &Database, alternate: Uuid, primary: Uuid) {
        exec(
            db,
            r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#,
            &[&guid_to_db(primary), &guid_to_db(alternate)],
        )
        .await;
    }

    /// Sets the user's resume position on an item's user-data row.
    async fn set_resume_position(db: &Database, user: Uuid, item: Uuid, ticks: i64) {
        exec(
            db,
            r#"UPDATE "UserData" SET "PlaybackPositionTicks" = ?1
               WHERE "UserId" = ?2 AND "ItemId" = ?3"#,
            &[&ticks.to_string(), &guid_to_db(user), &guid_to_db(item)],
        )
        .await;
    }

    #[tokio::test]
    async fn series_keys_ordered_by_recent_play() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let s1e1 = Uuid::new_v4();
        let s2e1 = Uuid::new_v4();
        seed_episode(&db, s1e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, s2e1, "series-b", 1, 1, false, Some(top)).await;
        // series-b watched most recently.
        seed_user_data(&db, user_id, s1e1, true, Some(ts("2020-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, s2e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let cutoff = "2000-01-01T00:00:00Z".parse().unwrap();
        let keys = svc
            .get_next_up_series_keys(&query_for(user, top), cutoff)
            .await
            .expect("keys");
        assert_eq!(keys, vec!["series-b".to_owned(), "series-a".to_owned()]);
    }

    /// The scope the manager hands over is the user's library folders; on a
    /// Jellyfin database those are virtual `CollectionFolder`s whose items hang
    /// off the `PhysicalFolderIds` in their `Data` blob — the
    /// `GetTopParentIdsForQuery` translation `LibraryManager.GetNextUpSeriesKeys`
    /// applies before the statement runs.
    #[tokio::test]
    async fn series_keys_scope_expands_a_collection_folder_to_its_physical_folders() {
        let db = test_db().await;
        let library = Uuid::new_v4();
        let physical = Uuid::new_v4();
        let other_physical = Uuid::new_v4();
        seed_item_with_data(
            &db,
            library,
            BaseItemKind::CollectionFolder,
            "Shows",
            &format!(r#"{{"PhysicalFolderIds":["{}"]}}"#, physical.simple()),
        )
        .await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let inside = Uuid::new_v4();
        let outside = Uuid::new_v4();
        seed_episode(&db, inside, "series-a", 1, 1, false, Some(physical)).await;
        seed_episode(&db, outside, "series-b", 1, 1, false, Some(other_physical)).await;
        seed_user_data(&db, user_id, inside, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(
            &db,
            user_id,
            outside,
            true,
            Some(ts("2021-01-01T00:00:00Z")),
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let keys = svc
            .get_next_up_series_keys(&query_for(user, library), ts("2000-01-01T00:00:00Z"))
            .await
            .expect("keys");
        assert_eq!(keys, vec!["series-a".to_owned()]);
    }

    #[tokio::test]
    async fn next_up_picks_episode_after_last_watched() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        // Watched e1 and e2 → next up is e3.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
        // The manager's `IUserDataManager` reads, answered from the batch: the
        // last watched row's play date, and no resume progress on the pick.
        let last = series
            .user_data
            .get(&guid_to_db(e2))
            .expect("last watched facts");
        assert!(last.played);
        assert_eq!(last.last_played_date, Some(ts("2021-01-02T00:00:00Z")));
        let next = series
            .user_data
            .get(&guid_to_db(e3))
            .expect("next up facts");
        assert!(!next.played);
        assert_eq!(next.playback_position_ticks, 0);
        assert!(next.last_played_date.is_none());
    }

    /// `DetermineNextEpisode` walks `nextEpisode.GetAllVersions()` for resume
    /// progress, so the position must be merged across the version group.
    #[tokio::test]
    async fn resume_position_on_an_alternate_version_reaches_the_primary() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e2_alt = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, e2_alt, "series-a", 1, 2, false, Some(top)).await;
        set_primary_version(&db, e2_alt, e2).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        // Half-way through the 4K version of episode 2.
        seed_user_data(
            &db,
            user_id,
            e2_alt,
            false,
            Some(ts("2021-01-02T00:00:00Z")),
        )
        .await;
        set_resume_position(&db, user_id, e2_alt, 5_000).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        let next_id = series
            .next_up
            .as_ref()
            .map(|e| e.id.clone())
            .expect("next up");
        let next = series.user_data.get(&next_id).expect("next up facts");
        assert_eq!(next.playback_position_ticks, 5_000);
        assert!(!next.played);
    }

    /// `ApplyAccessFiltering` keeps the batch rows to primaries (and owned
    /// rows only when they are extras): an alternate version is never the
    /// last watched, the next up or a special — even when only the alternate
    /// was played, the primary carries the propagated played state.
    #[tokio::test]
    async fn alternate_versions_and_owned_items_are_never_picks() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e1_alt = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e2_alt = Uuid::new_v4();
        let owned = Uuid::new_v4();
        let sp = Uuid::new_v4();
        let sp_alt = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e1_alt, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, e2_alt, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, owned, "series-a", 1, 3, false, Some(top)).await;
        seed_episode(&db, sp, "series-a", 0, 1, false, Some(top)).await;
        seed_episode(&db, sp_alt, "series-a", 0, 1, false, Some(top)).await;
        set_primary_version(&db, e1_alt, e1).await;
        set_primary_version(&db, e2_alt, e2).await;
        set_primary_version(&db, sp_alt, sp).await;
        exec(
            &db,
            r#"UPDATE "BaseItems" SET "OwnerId" = ?1 WHERE "Id" = ?2"#,
            &[&guid_to_db(e1), &guid_to_db(owned)],
        )
        .await;
        // An adopted Jellyfin database stores the ZERO GUID as "no owner" on
        // every row (Ferrofin's writer leaves it NULL); both mean unowned.
        exec(
            &db,
            r#"UPDATE "BaseItems" SET "OwnerId" = ?1 WHERE "Id" IN (?2, ?3)"#,
            &[&guid_to_db(Uuid::nil()), &guid_to_db(e2), &guid_to_db(sp)],
        )
        .await;
        seed_user_data(&db, user_id, e1, true, None).await;
        seed_user_data(&db, user_id, e1_alt, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                true,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series
                .specials
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec![guid_to_db(sp)]
        );
    }

    /// `GetMostRecentlyPlayedVersion` dates the last watched episode by the
    /// version that was played last, whichever row that is.
    #[tokio::test]
    async fn last_watched_date_comes_from_its_most_recently_played_version() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e1_alt = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e1_alt, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        set_primary_version(&db, e1_alt, e1).await;
        // The played state propagated to the primary carries no date; the
        // alternate that was actually played does.
        seed_user_data(&db, user_id, e1, true, None).await;
        seed_user_data(&db, user_id, e1_alt, true, Some(ts("2021-03-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        let last_id = series
            .last_watched
            .as_ref()
            .map(|e| e.id.clone())
            .expect("last watched");
        let last = series.user_data.get(&last_id).expect("last watched facts");
        assert_eq!(last.last_played_date, Some(ts("2021-03-01T00:00:00Z")));
    }

    /// A Jellyfin database holds one `UserData` row per user-data key (id,
    /// provider id, series position), so the projection sees the same episode
    /// more than once and must merge, not duplicate.
    #[tokio::test]
    async fn several_user_data_keys_of_one_episode_merge_into_one_reading() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        // Two keys for episode 1: the id key says played (dated), the
        // series-position key carries the resume position only.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        exec(
            &db,
            r#"INSERT INTO "UserData"
               ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "LastPlayedDate",
                "PlayCount", "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, '100001001', 0, NULL, 0, 777, 0)"#,
            &[&guid_to_db(e1), &guid_to_db(user_id)],
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        let last = series.user_data.get(&guid_to_db(e1)).expect("facts");
        assert!(last.played);
        assert_eq!(last.playback_position_ticks, 777);
        assert_eq!(last.last_played_date, Some(ts("2021-01-01T00:00:00Z")));
    }

    #[tokio::test]
    async fn specials_included_when_requested() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let special = Uuid::new_v4();
        let played_special = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, special, "series-a", 0, 1, false, Some(top)).await;
        seed_episode(&db, played_special, "series-a", 0, 2, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(
            &db,
            user_id,
            played_special,
            true,
            Some(ts("2021-01-03T00:00:00Z")),
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                true,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(series.specials.len(), 2);
        // The specials merge filters played specials through user data too.
        let facts = |id: Uuid| series.user_data.get(&guid_to_db(id)).copied();
        assert_eq!(facts(special).map(|f| f.played), Some(false));
        assert_eq!(facts(played_special).map(|f| f.played), Some(true));
    }

    // ── Semantics pinned below: which episode is "next", and over which pool ──

    /// Runs the batch with specials and rewatching off — the default shape.
    async fn batch_of(
        svc: &FerrofinNextUpService,
        user: UserEntity,
        top: Uuid,
        keys: &[&str],
    ) -> std::collections::HashMap<String, ferrofin_traits::persistence::NextUpEpisodeBatchResult>
    {
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        svc.get_next_up_episodes_batch(&query_for(user, top), &keys, false, false)
            .await
            .expect("batch")
    }

    #[tokio::test]
    async fn next_up_never_picks_a_virtual_episode() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2_virtual = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2_virtual, "series-a", 1, 2, true, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        // The unwatched season-1 episode 2 exists only as a virtual (missing
        // file) row, so the pick skips past it to episode 3.
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    #[tokio::test]
    async fn last_watched_position_counts_virtual_played_episodes() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2_virtual = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2_virtual, "series-a", 1, 2, true, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        // Only the *virtual* episode 2 is played; episode 1 is still unwatched.
        seed_user_data(
            &db,
            user_id,
            e2_virtual,
            true,
            Some(ts("2021-01-01T00:00:00Z")),
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        // The last-watched pool is *not* filtered to real rows, so the position
        // to search past is (1, 2) — episode 1 is behind it and episode 3 wins.
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2_virtual))
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    #[tokio::test]
    async fn unnumbered_last_watched_searches_from_the_start() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let watched = Uuid::new_v4();
        let unnumbered = Uuid::new_v4();
        seed_episode(&db, watched, "series-a", 1, 5, false, Some(top)).await;
        seed_episode(&db, unnumbered, "series-a", 1, 9, false, Some(top)).await;
        clear_index_number(&db, watched).await;
        clear_index_number(&db, unnumbered).await;
        seed_user_data(
            &db,
            user_id,
            watched,
            true,
            Some(ts("2021-01-01T00:00:00Z")),
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        // A half-known position yields no comparison point at all (`None`), so
        // the first unplayed candidate is taken even though it does not sort
        // strictly after the last-watched sort key.
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(unnumbered))
        );
    }

    #[tokio::test]
    async fn specials_are_not_next_up_candidates() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let special = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, special, "series-a", 0, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;

        let svc = FerrofinNextUpService::new(db.clone());

        // Nothing is played yet, so there is no last-watched position to search
        // past and the pick is simply the lowest-sorting candidate. Season 0
        // sorts *below* every real season, so the special would win outright
        // unless the pools exclude it: this is the shape where the season-0
        // guard — not the ordering — is load-bearing.
        let batch = batch_of(&svc, user.clone(), top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        assert!(series.last_watched.is_none(), "nothing has been played");
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e1)),
            "an unwatched series starts at its first real episode, not its special"
        );

        // And mid-series the special is still not a candidate.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn fully_watched_series_has_no_next_up() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        assert!(series.next_up.is_none());
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn each_series_in_a_batch_is_resolved_independently() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        seed_episode(&db, a1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, a2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, b1, "series-b", 2, 7, false, Some(top)).await;
        seed_episode(&db, b2, "series-b", 2, 8, false, Some(top)).await;
        seed_user_data(&db, user_id, a1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, b1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a", "series-b"]).await;
        assert_eq!(
            batch["series-a"].next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(a2))
        );
        assert_eq!(
            batch["series-b"].next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(b2))
        );
        // Neither series' pool leaks into the other's last-watched position.
        assert_eq!(
            batch["series-b"]
                .last_watched
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(b1))
        );
    }

    #[tokio::test]
    async fn repeated_series_key_still_yields_the_full_result() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let special = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, special, "series-a", 0, 1, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let keys = vec!["series-a".to_owned(), "series-a".to_owned()];
        let batch = svc
            .get_next_up_episodes_batch(&query_for(user, top), &keys, true, false)
            .await
            .expect("batch");
        assert_eq!(batch.len(), 1);
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(series.specials.len(), 1);
    }

    #[tokio::test]
    async fn rewatching_resumes_after_the_most_recently_played_episode() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        // Whole series watched, then episode 1 re-watched most recently.
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e3, true, Some(ts("2021-01-03T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-06-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                true,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert!(series.next_up.is_none(), "nothing is unwatched");
        // Highest position is still episode 3 …
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
        // … but the rewatch pick follows the most recent *play date* (episode 1).
        assert_eq!(
            series
                .last_watched_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(
            series
                .next_played_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn rewatching_never_picks_a_virtual_episode() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2_virtual = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2_virtual, "series-a", 1, 2, true, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        seed_user_data(
            &db,
            user_id,
            e2_virtual,
            true,
            Some(ts("2021-01-02T00:00:00Z")),
        )
        .await;
        seed_user_data(&db, user_id, e3, true, Some(ts("2021-01-03T00:00:00Z"))).await;
        // Episode 1 re-watched last, so the rewatch search starts after (1, 1).
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-06-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                true,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        // Episode 2 is played but virtual, so the rewatch pick skips to 3 — the
        // rewatch candidate pool is filtered to real rows even though the
        // last-watched pool is not.
        assert_eq!(
            series
                .next_played_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    /// Seeds `series-a` as watched through episode 2 with episode 1 re-watched
    /// most recently, so both rewatching fields have a real answer to report
    /// (`last_watched_for_rewatching` = e1, `next_played_for_rewatching` = e2),
    /// and returns `(user, top parent, e1, e2)`. Episode 3 stays unplayed so the
    /// non-rewatching `next_up` still has an answer too.
    async fn seed_rewatched_series(db: &Database) -> (UserEntity, Uuid, Uuid, Uuid) {
        let top = Uuid::new_v4();
        let user = seed_user(db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(db, e3, "series-a", 1, 3, false, Some(top)).await;
        seed_user_data(db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;
        seed_user_data(db, user_id, e1, true, Some(ts("2021-06-01T00:00:00Z"))).await;
        (user, top, e1, e2)
    }

    /// `corrupt_last_played_date` (in `test_support`) is what this rests on:
    /// the manager sorts the queue by the last watched episode's play date, so
    /// the projection must read `LastPlayedDate` on every batch — with or
    /// without rewatching — and a batch that fails against the corrupted column
    /// is proof the column was read.
    #[tokio::test]
    async fn last_played_date_is_read_on_every_batch() {
        let db = test_db().await;
        let (user, top, e1, _e2) = seed_rewatched_series(&db).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();
        corrupt_last_played_date(&db, user_id, e1).await;

        let svc = FerrofinNextUpService::new(db);
        let keys = ["series-a".to_owned()];
        for rewatching in [false, true] {
            let err = svc
                .get_next_up_episodes_batch(&query_for(user.clone(), top), &keys, false, rewatching)
                .await;
            assert!(
                err.is_err(),
                "the play date feeds the queue order, so it is read (rewatching = {rewatching})"
            );
        }
    }

    /// The *assembly* gate: the rewatching fields of the batch result are filled
    /// in exactly when rewatching was requested. Same database, same seeded
    /// history, only the flag differs.
    #[tokio::test]
    async fn rewatching_fields_follow_the_request_flag() {
        let db = test_db().await;
        let (user, top, e1, e2) = seed_rewatched_series(&db).await;

        let svc = FerrofinNextUpService::new(db);
        let keys = ["series-a".to_owned()];

        let batch = svc
            .get_next_up_episodes_batch(&query_for(user.clone(), top), &keys, false, false)
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert!(series.last_watched_for_rewatching.is_none());
        assert!(series.next_played_for_rewatching.is_none());
        assert!(series.specials.is_empty(), "specials were not requested");

        let batch = svc
            .get_next_up_episodes_batch(&query_for(user, top), &keys, false, true)
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series
                .last_watched_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(
            series
                .next_played_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        // The rewatching pick's resumable check reads its facts from the same map.
        assert!(series.user_data.contains_key(&guid_to_db(e2)));
    }

    #[tokio::test]
    async fn last_watched_and_rewatch_pick_can_be_the_same_row() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        // Episode 2 is both the highest played and the most recently played, so
        // `last_watched` and `last_watched_for_rewatching` name the same row.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                true,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series
                .last_watched_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert!(series.next_played_for_rewatching.is_none());
    }

    #[tokio::test]
    async fn series_keys_respect_the_date_cutoff_and_limit() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        seed_episode(&db, a, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, b, "series-b", 1, 1, false, Some(top)).await;
        seed_episode(&db, c, "series-c", 1, 1, false, Some(top)).await;
        seed_user_data(&db, user_id, a, true, Some(ts("2019-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, b, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, c, true, Some(ts("2022-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let mut query = query_for(user, top);
        query.limit = Some(1);
        let keys = svc
            .get_next_up_series_keys(&query, ts("2020-01-01T00:00:00Z"))
            .await
            .expect("keys");
        // series-a is behind the cutoff; the limit keeps only the newest of the
        // remaining two, newest-first.
        assert_eq!(keys, vec!["series-c".to_owned()]);
    }
}
