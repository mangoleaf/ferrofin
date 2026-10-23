//! Port of `SeriesMetadataService.RemoveObsoleteEpisodes` (v12.2), the step
//! of every series refresh that deletes a series' virtual (missing) episodes
//! once they stand for nothing missing.

use std::collections::{HashMap, HashSet};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use uuid::Uuid;

use super::{LibraryScanner, ScanRun, index_number_end};

/// What the rules read of one of a series' episodes.
#[derive(Debug, Clone)]
pub(super) struct Episode {
    pub(super) id: Uuid,
    pub(super) season: Option<i64>,
    pub(super) number: Option<i64>,
    pub(super) number_end: Option<i64>,
    /// Neither `IsVirtualItem` nor `IsMissingEpisode` (no path).
    pub(super) physical: bool,
    pub(super) provider_ids: Vec<(String, String)>,
}

impl Episode {
    /// `Episode.ContainsEpisodeNumber`.
    fn contains(&self, number: i64) -> bool {
        match (self.number, self.number_end) {
            (Some(start), Some(end)) => (start..=end).contains(&number),
            (Some(start), None) => start == number,
            _ => false,
        }
    }
}

/// `SeriesMetadataService.IdentityKey`, compared ignoring case as upstream's
/// `StringComparer.OrdinalIgnoreCase` set does.
fn identity_key(provider: &str, id: &str) -> String {
    format!("{provider}={id}").to_lowercase()
}

/// `SeriesMetadataService.GetPresentEpisodeIds`: the provider ids held by
/// exactly one physical episode. One held by several (a series' own IMDb id
/// on every episode, TMDb's `0` for an unknown TVRage id) names no episode.
pub(super) fn present_episode_ids(episodes: &[Episode]) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for episode in episodes.iter().filter(|e| e.physical) {
        for (provider, id) in &episode.provider_ids {
            if id.trim().is_empty() {
                continue;
            }
            *counts.entry(identity_key(provider, id)).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|&(_, count)| count == 1)
        .map(|(key, _)| key)
        .collect()
}

/// `SeriesMetadataService.IsAlreadyPresent`.
pub(super) fn is_already_present(episode: &Episode, present: &HashSet<String>) -> bool {
    episode.provider_ids.iter().any(|(provider, id)| {
        !id.trim().is_empty() && present.contains(&identity_key(provider, id))
    })
}

/// The virtual episodes `RemoveObsoleteEpisodes` deletes, each with its
/// reason: per season number, one with no episode number, one whose number a
/// physical episode of that season covers, and one carrying a provider id a
/// physical episode anywhere in the series holds — aired, DVD and absolute
/// orders number the same episode differently.
pub(super) fn obsolete_episodes(episodes: &[Episode]) -> Vec<(&Episode, &'static str)> {
    let present = present_episode_ids(episodes);
    let mut seasons: Vec<Option<i64>> = Vec::new();
    for episode in episodes {
        if !seasons.contains(&episode.season) {
            seasons.push(episode.season);
        }
    }
    let mut out = Vec::new();
    for season in seasons {
        let (physical, virtual_): (Vec<&Episode>, Vec<&Episode>) = episodes
            .iter()
            .filter(|e| e.season == season)
            .partition(|e| e.physical);
        for episode in virtual_ {
            let Some(number) = episode.number else {
                out.push((episode, "it has no episode number"));
                continue;
            };
            if physical.iter().any(|e| e.contains(number)) {
                out.push((episode, "a physical episode has that number"));
            } else if is_already_present(episode, &present) {
                out.push((
                    episode,
                    "the same episode is already present under another number",
                ));
            }
        }
    }
    out
}

/// At most one virtual episode, of the series keyed `key` or of any: whether
/// there is anything to remove. Every missing episode is stored virtual
/// (Jellyfin's providers set `IsVirtualItem` on each one they add, and a
/// scan plans no path-less episode), so the probe finds them all.
fn virtual_episodes(key: Option<String>) -> InternalItemsQuery {
    InternalItemsQuery {
        include_item_types: vec![BaseItemKind::Episode],
        is_virtual_item: Some(true),
        series_presentation_unique_key: key,
        limit: Some(1),
        ..Default::default()
    }
}

/// Neither `IsVirtualItem` nor `IsMissingEpisode`, which is `LocationType ==
/// Virtual`: no path. A channel's path-less item is `Remote`, but no channel
/// item is a library series' episode.
fn is_physical(row: &BaseItemEntity) -> bool {
    !row.is_virtual_item && row.path.as_deref().is_some_and(|p| !p.is_empty())
}

impl Episode {
    /// The facts of a stored episode row, with its provider ids.
    fn of(row: &BaseItemEntity, provider_ids: Vec<(String, String)>) -> Option<Self> {
        Some(Self {
            id: Uuid::parse_str(&row.id).ok()?,
            season: row.parent_index_number,
            number: row.index_number,
            number_end: index_number_end(row).map(i64::from),
            physical: is_physical(row),
            provider_ids,
        })
    }
}

impl LibraryScanner {
    /// `RemoveObsoleteEpisodes` for each of `series`, the series this scan
    /// refreshed: their obsolete virtual episodes are deleted (no file is
    /// touched; a virtual episode has none). Run after the walk saved every
    /// row, so an episode file the walk just added already counts.
    pub(super) async fn remove_obsolete_episodes(
        &self,
        series: &[Uuid],
        run: ScanRun<'_>,
    ) -> Result<(), ServiceError> {
        let Some(items) = &self.item_repository else {
            return Ok(());
        };
        if series.is_empty() {
            return Ok(());
        }
        // A library with no missing episode has nothing to remove: one
        // id-only probe says so.
        if items
            .get_item_ids(&virtual_episodes(None))
            .await?
            .is_empty()
        {
            return Ok(());
        }
        let mut removed = 0_usize;
        for series_row in items.retrieve_items(series).await? {
            if run.cancel.is_cancelled() {
                break;
            }
            // The key its episodes carry, as the episode list reads it: its
            // own, else its id.
            let key = series_row
                .presentation_unique_key
                .clone()
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| series_row.id.clone());
            // One series failing leaves the others to go on, as a failed
            // series refresh does upstream; the next scan retries it.
            match self
                .remove_series_obsolete_episodes(items.as_ref(), &series_row, key)
                .await
            {
                Ok(count) => removed += count,
                Err(err) => tracing::warn!(
                    item_id = %series_row.id,
                    %err,
                    "could not remove the series' obsolete virtual episodes; the next scan retries"
                ),
            }
        }
        if removed > 0 {
            tracing::info!(removed, "removed obsolete virtual episodes");
        }
        Ok(())
    }

    /// One series' `RemoveObsoleteEpisodes`, over `Series.GetEpisodes(null,
    /// …, true)`: every episode carrying its `key`, the missing ones
    /// included. Returns how many were deleted.
    async fn remove_series_obsolete_episodes(
        &self,
        items: &dyn ferrofin_traits::persistence::ItemRepository,
        series_row: &BaseItemEntity,
        key: String,
    ) -> Result<usize, ServiceError> {
        // Only a series with a virtual episode has anything to remove.
        if items
            .get_item_ids(&virtual_episodes(Some(key.clone())))
            .await?
            .is_empty()
        {
            return Ok(0);
        }
        let rows = items
            .get_item_list(&InternalItemsQuery {
                series_presentation_unique_key: Some(key),
                include_item_types: vec![BaseItemKind::Episode],
                ..Default::default()
            })
            .await?;
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        let mut provider_ids = self.persistence.provider_ids_for_items(&ids).await?;
        let episodes: Vec<Episode> = rows
            .iter()
            .filter_map(|row| {
                let ids = Uuid::parse_str(&row.id)
                    .ok()
                    .and_then(|id| provider_ids.remove(&id))
                    .unwrap_or_default();
                Episode::of(row, ids)
            })
            .collect();
        let obsolete = obsolete_episodes(&episodes);
        if obsolete.is_empty() {
            return Ok(0);
        }
        for (episode, reason) in &obsolete {
            tracing::debug!(
                item_id = %episode.id,
                series_name = series_row.name.as_deref().unwrap_or_default(),
                season_number = episode.season,
                episode_number = episode.number,
                reason,
                "removing a virtual episode"
            );
        }
        let doomed: Vec<Uuid> = obsolete.iter().map(|(episode, _)| episode.id).collect();
        Ok(self.persistence.delete_items(&doomed).await?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(number: i64, physical: bool, ids: &[(&str, &str)]) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            season: Some(3),
            number: Some(number),
            number_end: None,
            physical,
            provider_ids: ids
                .iter()
                .map(|&(provider, id)| (provider.to_owned(), id.to_owned()))
                .collect(),
        }
    }

    fn physical(number: i64, ids: &[(&str, &str)]) -> Episode {
        episode(number, true, ids)
    }

    fn virtual_(number: i64, ids: &[(&str, &str)]) -> Episode {
        episode(number, false, ids)
    }

    // Upstream `SeriesMetadataServiceTests` (v12.2), transliterated.

    #[test]
    fn is_already_present_virtual_episode_carries_the_id_of_a_present_one() {
        // The Six Million Dollar Man S3: 21 files numbered in DVD order, 23
        // episodes in aired order, so aired 22 and 23 are the files stored
        // as 20 and 21.
        let present = present_episode_ids(&[
            physical(20, &[("Tvdb", "236715")]),
            physical(21, &[("Tvdb", "236716")]),
        ]);
        assert!(is_already_present(
            &virtual_(22, &[("Tvdb", "236715")]),
            &present
        ));
        assert!(is_already_present(
            &virtual_(23, &[("Tvdb", "236716")]),
            &present
        ));
    }

    #[test]
    fn is_already_present_genuinely_missing_episode() {
        let present = present_episode_ids(&[
            physical(20, &[("Tvdb", "236715")]),
            physical(21, &[("Tvdb", "236716")]),
        ]);
        assert!(!is_already_present(
            &virtual_(15, &[("Tvdb", "4273533")]),
            &present
        ));
    }

    #[test]
    fn present_episode_ids_id_shared_by_several_episodes_is_not_an_identity() {
        let present = present_episode_ids(&[
            physical(
                1,
                &[("Imdb", "tt0836592"), ("TvRage", "0"), ("Tvdb", "1001")],
            ),
            physical(
                2,
                &[("Imdb", "tt0836592"), ("TvRage", "0"), ("Tvdb", "1002")],
            ),
        ]);
        assert!(!is_already_present(
            &virtual_(3, &[("Imdb", "tt0836592")]),
            &present
        ));
        assert!(!is_already_present(
            &virtual_(4, &[("TvRage", "0")]),
            &present
        ));
        assert!(is_already_present(
            &virtual_(5, &[("Tvdb", "1001")]),
            &present
        ));
    }

    #[test]
    fn present_episode_ids_virtual_episode_does_not_count_as_present() {
        assert!(present_episode_ids(&[virtual_(22, &[("Tvdb", "236715")])]).is_empty());
    }

    #[test]
    fn present_episode_ids_blank_stored_id_is_ignored() {
        assert!(present_episode_ids(&[physical(1, &[("Tvdb", " ")])]).is_empty());
    }

    // `RemoveObsoleteEpisodes`'s own rules.

    #[test]
    fn a_virtual_episode_goes_when_a_physical_one_covers_its_number() {
        let mut double = physical(4, &[]);
        double.number_end = Some(5);
        let numberless = Episode {
            number: None,
            ..virtual_(0, &[])
        };
        let episodes = [
            double,
            virtual_(4, &[]),
            virtual_(5, &[]),
            virtual_(6, &[]),
            numberless,
            // Same number, another season: not covered.
            Episode {
                season: Some(4),
                ..virtual_(4, &[])
            },
            // A DVD-numbered file carries the aired-order episode's id.
            physical(20, &[("Tvdb", "236715")]),
            Episode {
                season: Some(5),
                ..virtual_(22, &[("TVDB", "236715")])
            },
        ];
        let reasons: Vec<_> = obsolete_episodes(&episodes)
            .into_iter()
            .map(|(e, reason)| (e.season, e.number, reason))
            .collect();
        assert_eq!(
            reasons,
            [
                (Some(3), Some(4), "a physical episode has that number"),
                (Some(3), Some(5), "a physical episode has that number"),
                (Some(3), None, "it has no episode number"),
                (
                    Some(5),
                    Some(22),
                    "the same episode is already present under another number"
                ),
            ]
        );
    }

    #[test]
    fn physical_episodes_are_never_removed() {
        let numberless = Episode {
            number: None,
            ..physical(0, &[])
        };
        assert!(obsolete_episodes(&[numberless, physical(1, &[]), physical(1, &[])]).is_empty());
    }
}
