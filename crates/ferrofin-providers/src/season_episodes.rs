//! `Season.GetEpisodes` (`MediaBrowser.Controller/Entities/TV/Season.cs:196-219`,
//! `Series.GetSeasonEpisodes`/`FilterEpisodesBySeason`, `Series.cs:412-504`):
//! the episodes of one season as upstream's season refresh, its listing and
//! its children-derived metadata read them. Here, where the library manager,
//! the library scan and the provider manager's refresh all reach it.

use std::collections::HashMap;

use ferrofin_db::entities::base_items::{
    BaseItemEntity, aired_episode_order, is_virtual_location, presentation_key, relation_id,
};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_util::stable_sort::stable_sort_by;
use uuid::Uuid;

/// `Season.GetEpisodes` → `Series.GetSeasonEpisodes(parentSeason, user, options,
/// shouldIncludeMissingEpisodes: true)`: the series' episodes (its seasons
/// alongside, to resolve each episode's parent) narrowed to `season`'s by
/// `FilterEpisodesBySeason` — with `include_specials`
/// (`DisplaySpecialsWithinSeasons`), a special airing within a numbered
/// season belongs to it — and sorted in aired order (season 0 keeps the
/// series' `SortName` order). Empty when the season has no series; a series
/// row that fails to read is an error here, which the scan's
/// [`series_season_episode_rows`] caller counts as no series instead.
/// `season_id` is the season's id; `query` supplies the user and the DTO
/// options the series query runs with. The series' rows are read with only
/// the columns the selection reads, and the full rows of the season's
/// episodes after it (gl-1259).
///
/// # Errors
///
/// The item store's, reading the series or its episodes.
pub async fn season_episode_rows(
    items: &dyn ItemRepository,
    season_id: Uuid,
    season: BaseItemEntity,
    query: &InternalItemsQuery,
    include_specials: bool,
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    let series = match season_series_column(&season).and_then(|id| relation_id(Some(id))) {
        Some(id) => items.retrieve_item(id).await?,
        None => None,
    };
    series_season_episode_rows(items, season_id, season, series, query, include_specials).await
}

/// The column naming `season`'s series (`Season.Series`): its `SeriesId`,
/// else its `ParentId`, skipping one [`relation_id`] reads as no relation.
#[must_use]
pub fn season_series_column(season: &BaseItemEntity) -> Option<&str> {
    [season.series_id.as_deref(), season.parent_id.as_deref()]
        .into_iter()
        .flatten()
        .find(|id| relation_id(Some(id)).is_some())
}

/// [`season_episode_rows`] with `series`, the row [`season_series_column`]
/// names, already read (`Season.GetEpisodes(Series series, …)`). Empty when
/// it is `None` or not a series.
///
/// # Errors
///
/// The item store's, reading the series' episodes and seasons.
#[allow(clippy::too_many_lines)] // source relation resolution, then selection
pub async fn series_season_episode_rows(
    items: &dyn ItemRepository,
    season_id: Uuid,
    season: BaseItemEntity,
    series: Option<BaseItemEntity>,
    query: &InternalItemsQuery,
    include_specials: bool,
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    let Some(series) = series.filter(|row| row.type_.rsplit('.').next() == Some("Series")) else {
        return Ok(Vec::new());
    };
    let series_id = relation_id(Some(&series.id));
    let rows = items
        .get_episode_order_rows(&InternalItemsQuery {
            user: query.user.clone(),
            series_presentation_unique_key: Some(presentation_key(&series).to_owned()),
            include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Season],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Ascending,
            )],
            dto_options: query.dto_options.clone(),
            // Season.GetItemsInternal passes shouldIncludeMissing=true.
            ..Default::default()
        })
        .await?;
    let series_seasons: Vec<_> = rows
        .iter()
        .filter(|row| row.type_.rsplit('.').next() == Some("Season"))
        .cloned()
        .collect();
    let mut seasons: HashMap<_, _> = series_seasons
        .iter()
        .filter_map(|row| relation_id(Some(&row.id)).map(|id| (id, row.clone())))
        .collect();
    seasons.insert(season_id, season.clone());
    let mut unknown = std::collections::HashSet::new();
    for row in &rows {
        if row.type_.rsplit('.').next() == Some("Episode")
            && let Some(id) = relation_id(row.season_id.as_deref())
                .or_else(|| relation_id(row.parent_id.as_deref()))
            && !seasons.contains_key(&id)
        {
            unknown.insert(id);
        }
    }
    if !unknown.is_empty() {
        for row in items
            .retrieve_items(&unknown.into_iter().collect::<Vec<_>>())
            .await?
        {
            if row.type_.rsplit('.').next() == Some("Season")
                && let Some(id) = relation_id(Some(&row.id))
            {
                seasons.insert(id, row);
            }
        }
    }
    let key = presentation_key(&season);
    let number = season.index_number;
    let support_specials = include_specials && number.is_some_and(|number| number != 0);
    let mut matches = Vec::new();
    for row in rows
        .into_iter()
        .filter(|row| row.type_.rsplit('.').next() == Some("Episode"))
    {
        let parent = relation_id(row.season_id.as_deref())
            .or_else(|| relation_id(row.parent_id.as_deref()))
            .and_then(|id| seasons.get(&id))
            .or_else(|| {
                (relation_id(row.parent_id.as_deref()) == series_id)
                    .then(|| {
                        series_seasons
                            .iter()
                            .find(|parent| parent.index_number == row.parent_index_number)
                    })
                    .flatten()
            });
        let same_season = parent.is_some_and(|parent| {
            ferrofin_util::string_extensions::equals_ordinal_ignore_case(
                presentation_key(parent),
                key,
            )
        });
        // The disabled source query is restricted to the season's
        // presentation ancestry, before FilterEpisodesBySeason runs.
        if !include_specials && !same_season {
            continue;
        }
        let order = aired_episode_order(&row);
        let current = if support_specials {
            order
                .airs_after_season
                .or(order.airs_before_season)
                .or(order.season)
        } else {
            order.season
        };
        let matches_number = current.is_some() && number.is_some() && current == number;
        let matches_virtual = current.is_none()
            && number.is_none()
            && is_virtual_location(&season)
            && parent.is_none_or(is_virtual_location);
        if matches_number || matches_virtual || same_season {
            matches.push((order, row));
        }
    }
    if number != Some(0) {
        // `LibraryManager.Sort`'s LINQ `OrderBy`: stable, and the aired-order
        // comparer is not a total order, on which `sort_by` may panic.
        stable_sort_by(&mut matches, |(left, _), (right, _)| left.compare(right));
    }
    let ids: Vec<Uuid> = matches
        .iter()
        .filter_map(|(_, row)| Uuid::parse_str(&row.id).ok())
        .collect();
    let mut full: HashMap<Uuid, BaseItemEntity> = items
        .retrieve_items(&ids)
        .await?
        .into_iter()
        .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
        .collect();
    Ok(ids.iter().filter_map(|id| full.remove(id)).collect())
}
