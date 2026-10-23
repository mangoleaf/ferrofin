//! The decision half of `GetLatestTvShowItems` (steps 3 and 5), on in-memory
//! episodes: which card each series gets.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::{LatestCard, LatestEpisode, analyse_series, choose_cards};

fn at(hours: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2024-02-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::hours(hours)
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// An episode of series `series` (named after it), in season `season`.
fn episode(n: u128, series: u128, season: u128, hours: i64) -> LatestEpisode {
    LatestEpisode {
        id: id(n),
        series_name: format!("series {series}"),
        date_created: Some(at(hours)),
        season_id: Some(id(season)),
        series_id: Some(id(series)),
    }
}

/// `episodes` newest first, as step 2 reads them.
fn cards(
    mut episodes: Vec<LatestEpisode>,
    season_episodes: &[(u128, i64)],
    series_seasons: &[(u128, i64)],
) -> Vec<LatestCard> {
    episodes.sort_by(|a, b| b.date_created.cmp(&a.date_created).then(b.id.cmp(&a.id)));
    let seasons: HashMap<Uuid, i64> = season_episodes.iter().map(|(s, c)| (id(*s), *c)).collect();
    let series: HashMap<Uuid, i64> = series_seasons.iter().map(|(s, c)| (id(*s), *c)).collect();
    choose_cards(&analyse_series(&episodes), &seasons, &series)
}

fn card(season: Option<u128>, series: Option<u128>, hours: i64, episode: u128) -> LatestCard {
    LatestCard {
        season_id: season.map(id),
        series_id: series.map(id),
        max_date: at(hours),
        most_recent_episode_id: id(episode),
    }
}

/// One recent episode of a multi-season series is the Episode; an older one
/// outside the 24-hour window does not count.
#[test]
fn one_recent_episode_is_the_episode() {
    assert_eq!(
        cards(
            vec![episode(11, 1, 100, 100), episode(10, 1, 101, 0)],
            &[(100, 10), (101, 10)],
            &[(1, 2)],
        ),
        [card(None, None, 100, 11)]
    );
}

/// Several recent episodes in one season of a multi-season series are the
/// Season; in the only season, the Series.
#[test]
fn several_in_one_season_are_the_season_or_the_series() {
    let recent = vec![episode(11, 1, 100, 10), episode(12, 1, 100, 12)];
    assert_eq!(
        cards(recent.clone(), &[(100, 10)], &[(1, 3)]),
        [card(Some(100), None, 12, 12)]
    );
    assert_eq!(
        cards(recent, &[(100, 10)], &[(1, 1)]),
        [card(None, Some(1), 12, 12)]
    );
}

/// A lone recent episode that is the whole season counts as all of it: the
/// Season of a multi-season series.
#[test]
fn a_whole_one_episode_season_is_the_season() {
    assert_eq!(
        cards(vec![episode(11, 1, 100, 5)], &[(100, 1)], &[(1, 2)]),
        [card(Some(100), None, 5, 11)]
    );
}

/// Recent episodes across seasons are the Series; with no season at all,
/// the Episode.
#[test]
fn several_seasons_are_the_series_and_none_is_the_episode() {
    assert_eq!(
        cards(
            vec![episode(11, 1, 100, 1), episode(12, 1, 101, 2)],
            &[],
            &[(1, 2)],
        ),
        [card(None, Some(1), 2, 12)]
    );
    let mut loose = episode(11, 1, 100, 1);
    loose.season_id = None;
    let mut looser = episode(12, 1, 100, 2);
    looser.season_id = None;
    assert_eq!(
        cards(vec![loose, looser], &[], &[(1, 2)]),
        [card(None, None, 2, 12)]
    );
}

/// Each series is analysed apart, in the order its newest episode comes.
#[test]
fn series_are_analysed_apart_newest_first() {
    assert_eq!(
        cards(
            vec![
                episode(11, 1, 100, 1),
                episode(21, 2, 200, 30),
                episode(22, 2, 200, 31),
            ],
            &[(100, 5), (200, 5)],
            &[(1, 2), (2, 1)],
        ),
        [card(None, Some(2), 31, 22), card(None, None, 1, 11)]
    );
}

/// Step 4's counts seek the `SeasonId` / `SeriesId` index for each id rather
/// than walking every row of a type: the `+"Type"` term keeps a
/// `Type`-leading index out of the plan.
#[tokio::test]
async fn child_counts_seek_the_parent_id_index() {
    let db = crate::test_support::test_db().await;
    for (column, kind) in [
        ("SeasonId", ferrofin_model::data::BaseItemKind::Episode),
        ("SeriesId", ferrofin_model::data::BaseItemKind::Season),
    ] {
        let plan = super::explain(
            &db,
            super::count_children_query(column, kind, &[id(1), id(2)]),
        )
        .await;
        assert!(plan.contains(&format!("({column}=?)")), "{plan}");
        assert!(!plan.contains("SCAN b"), "{plan}");
    }
}

/// Steps 1 and 2 read a library's rows through an index on the library, as
/// the filter jellyfin-web's "Latest in TV" row builds scopes them, never by
/// walking every row of the database.
#[tokio::test]
async fn latest_tv_statements_seek_the_library() {
    let db = crate::test_support::test_db().await;
    let filter = ferrofin_traits::options::InternalItemsQuery {
        is_folder: Some(false),
        is_virtual_item: Some(false),
        media_types: vec![ferrofin_model::data::MediaType::Video],
        top_parent_ids: vec![id(0x900)],
        limit: Some(16),
        ..Default::default()
    };
    let series = super::explain(
        &db,
        crate::translate_query::build_latest_tv_series_query(&filter),
    )
    .await;
    assert!(series.contains("SEARCH bi USING"), "{series}");
    assert!(!series.contains("SCAN bi"), "{series}");
    let episodes = super::explain(
        &db,
        crate::translate_query::build_latest_tv_episodes_query(
            &filter,
            &["A".to_owned(), "B".to_owned()],
            Some(at(0)),
        ),
    )
    .await;
    assert!(episodes.contains("SEARCH bi USING"), "{episodes}");
    assert!(!episodes.contains("SCAN bi"), "{episodes}");
}

/// The window is inclusive: an episode exactly 24 hours older than the
/// series' newest still counts as added with it.
#[test]
fn the_window_includes_its_boundary() {
    let analysis = analyse_series(&[episode(12, 1, 100, 24), episode(11, 1, 100, 0)]);
    assert_eq!(analysis[0].recent_episode_count, 2);
}

/// A series whose newest episode has no `DateCreated` (so none has one) is
/// dated the minimum and shows its newest episode; nothing panics on the
/// window below it.
#[test]
fn an_undated_series_shows_its_episode() {
    let mut undated = episode(11, 1, 100, 0);
    undated.date_created = None;
    let analysis = analyse_series(&[undated]);
    assert_eq!(analysis[0].max_date, DateTime::<Utc>::MIN_UTC);
    assert_eq!(analysis[0].recent_episode_count, 0);
    assert_eq!(
        choose_cards(&analysis, &HashMap::new(), &HashMap::new())[0],
        LatestCard {
            season_id: None,
            series_id: None,
            max_date: DateTime::<Utc>::MIN_UTC,
            most_recent_episode_id: id(11),
        }
    );
}
