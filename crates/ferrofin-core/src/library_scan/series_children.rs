//! The series' children its refresh reaches that the walk plans none of:
//! port of `Series.RefreshAllMetadata` (`MediaBrowser.Controller/Entities/TV/
//! Series.cs:333-406`), which library validation and a series' own refresh
//! (`Folder.RefreshAllMetadataForContainer`, `Folder.cs:838-846`) run after
//! the series' metadata refresh: every season of the series, then every
//! episode, each through its own metadata service with the refresh's
//! options.
//!
//! The walk refreshes the seasons and episodes it plans, which are those with
//! a path and the path-less seasons its loose episode files group into
//! (`plan_loose_episodes`). What is left are the path-less children: a
//! virtual season, a path-less season whose files are gone, a missing
//! episode. Those refresh here through the provider manager, after the TMDb
//! missing-episode provider (and, in a library validation,
//! `RemoveObsoleteEpisodes`), which upstream runs within the series' own
//! refresh, before its children.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use ferrofin_db::entities::base_items::{BaseItemEntity, is_virtual_location};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

use super::{LibraryScanner, Planned, ScanRun};

/// The age past which `RefreshAllMetadataInternal` leaves a missing episode
/// out of a refresh that neither runs in full nor replaces all metadata:
/// upstream's hardcoded 30 days (`Series.cs:386-392`).
const MISSING_EPISODE_REFRESH_DAYS: i64 = 30;

/// `RefreshAllMetadataInternal`'s `skipItem`: a missing (path-less) episode
/// that aired more than [`MISSING_EPISODE_REFRESH_DAYS`] before `now`, under
/// `options` that neither run in full nor replace all metadata.
fn skips_missing_episode(
    episode: &BaseItemEntity,
    options: &MetadataRefreshOptions,
    now: DateTime<Utc>,
) -> bool {
    options.metadata_refresh_mode != MetadataRefreshMode::FullRefresh
        && !options.replace_all_metadata
        && is_virtual_location(episode)
        && episode
            .premiere_date
            .is_some_and(|aired| now - aired > Duration::days(MISSING_EPISODE_REFRESH_DAYS))
}

impl LibraryScanner {
    /// `RefreshAllMetadata` of each of `series`, the series this scan
    /// refreshes, for the children the walk did not refresh: their
    /// path-less seasons not in `planned`, then their missing episodes but
    /// those [`skips_missing_episode`] leaves out, each through the provider
    /// manager with the scan's options, an Identify's chosen result
    /// included: [`refresh_single_item`] hands it to a season or an episode
    /// as its series' ids (`ApplySearchResult`). A failed refresh is logged
    /// and the next child refreshed; a cancelled scan stops between two.
    ///
    /// [`refresh_single_item`]: ferrofin_traits::providers::ProviderManager::refresh_single_item
    ///
    /// # Errors
    ///
    /// The item store's, listing the children.
    pub(super) async fn refresh_pathless_series_children(
        &self,
        series: &[Uuid],
        planned: &[Planned],
        run: ScanRun<'_>,
    ) -> Result<(), ServiceError> {
        let (Some(items), Some(providers)) = (&self.item_repository, &self.metadata_savers) else {
            return Ok(());
        };
        let options = run.options;
        let planned: HashSet<Uuid> = planned.iter().map(|p| p.id).collect();
        let now = Utc::now();
        for chunk in series.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            // Seasons first, then episodes, as `RefreshAllMetadataInternal`
            // orders them. shortcut: only virtual episodes are read, which
            // keeps the episodes with a file (all planned) out of the read; a
            // path-less episode not flagged virtual would be missed, and
            // neither the scan nor the missing-episode provider stores one.
            for (kind, is_virtual_item) in [
                (BaseItemKind::Season, None),
                (BaseItemKind::Episode, Some(true)),
            ] {
                let children = items
                    .get_episode_order_rows(&InternalItemsQuery {
                        include_item_types: vec![kind],
                        ancestor_ids: chunk.to_vec(),
                        is_virtual_item,
                        group_by_presentation_unique_key: false,
                        ..InternalItemsQuery::default()
                    })
                    .await?;
                for child in children {
                    if run.cancel.is_cancelled() {
                        return Ok(());
                    }
                    let Ok(id) = Uuid::parse_str(&child.id) else {
                        continue;
                    };
                    if !is_virtual_location(&child)
                        || planned.contains(&id)
                        || (kind == BaseItemKind::Episode
                            && skips_missing_episode(&child, options, now))
                    {
                        continue;
                    }
                    if let Err(err) = providers.refresh_single_item(id, options).await {
                        tracing::warn!(%err, item_id = %id, "could not refresh a series' path-less season or episode");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `skipItem`'s cases: only a path-less episode that aired more than 30
    /// days ago, under a refresh that neither runs in full nor replaces all
    /// metadata.
    #[rstest::rstest]
    #[case::aired_long_ago(None, Some(31), MetadataRefreshMode::Default, false, true)]
    #[case::aired_within_the_month(None, Some(29), MetadataRefreshMode::Default, false, false)]
    #[case::unaired(None, Some(-7), MetadataRefreshMode::Default, false, false)]
    #[case::undated(None, None, MetadataRefreshMode::Default, false, false)]
    #[case::with_a_file(
        Some("/tv/S/S S01E01.mkv"),
        Some(31),
        MetadataRefreshMode::Default,
        false,
        false
    )]
    #[case::full_refresh(None, Some(31), MetadataRefreshMode::FullRefresh, false, false)]
    #[case::replace_all_metadata(None, Some(31), MetadataRefreshMode::Default, true, false)]
    #[case::validation_only(None, Some(31), MetadataRefreshMode::ValidationOnly, false, true)]
    fn a_missing_episode_aired_over_a_month_ago_is_skipped(
        #[case] path: Option<&str>,
        #[case] aired_days_ago: Option<i64>,
        #[case] mode: MetadataRefreshMode,
        #[case] replace_all_metadata: bool,
        #[case] skipped: bool,
    ) {
        let now = Utc::now();
        let episode = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Episode".to_owned(),
            path: path.map(str::to_owned),
            premiere_date: aired_days_ago.map(|days| now - Duration::days(days)),
            ..BaseItemEntity::default()
        };
        let options = MetadataRefreshOptions {
            metadata_refresh_mode: mode,
            replace_all_metadata,
            ..MetadataRefreshOptions::default()
        };
        assert_eq!(skips_missing_episode(&episode, &options, now), skipped);
    }
}
