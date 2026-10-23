//! The `IsAiring` filter: v12.2 `BaseItemRepository.TranslateQuery.cs:433-443`.
//! `true` keeps what is on now (`StartDate <= now && EndDate >= now`); `false`
//! keeps what is not (`StartDate > now || EndDate < now`, the negation
//! jellyfin/jellyfin#17588 fixed in v12.0-rc5). Both the item query and the
//! by-name content scope `/Items/Filters2` builds its facets from apply it.

use super::*;
use crate::test_support::{
    fetch_item, item_repository_over, save_item, seed_item_genre, seed_library_over,
    seed_named_item, test_db,
};

const ENDED: Uuid = Uuid::from_u128(0x0224_0001);
const AIRING: Uuid = Uuid::from_u128(0x0224_0002);
const UPCOMING: Uuid = Uuid::from_u128(0x0224_0003);

/// Three programmes: one that ended an hour ago, one on now, one starting in
/// an hour, each with its own genre.
async fn seeded() -> Database {
    let db = test_db().await;
    let now = chrono::Utc::now();
    let hour = chrono::Duration::hours(1);
    for (id, name, start, end) in [
        (ENDED, "Ended", now - hour * 2, now - hour),
        (AIRING, "Airing", now - hour, now + hour),
        (UPCOMING, "Upcoming", now + hour, now + hour * 2),
    ] {
        seed_named_item(&db, id, BaseItemKind::LiveTvProgram, name).await;
        let mut row = fetch_item(&db, id).await;
        row.start_date = Some(start);
        row.end_date = Some(end);
        save_item(&db, &row).await;
        seed_item_genre(&db, id, name).await;
    }
    seed_library_over(&db, &[ENDED, AIRING, UPCOMING]).await;
    db
}

fn programmes(is_airing: bool) -> InternalItemsQuery {
    InternalItemsQuery {
        include_item_types: vec![BaseItemKind::LiveTvProgram],
        is_airing: Some(is_airing),
        ..InternalItemsQuery::default()
    }
}

#[rstest::rstest]
#[case::not_airing(false, vec![ENDED, UPCOMING])]
#[case::airing(true, vec![AIRING])]
#[tokio::test]
async fn the_item_query_splits_programmes_on_whether_they_are_on_now(
    #[case] is_airing: bool,
    #[case] expected: Vec<Uuid>,
) {
    let db = seeded().await;
    let repository = item_repository_over(db.clone());

    let mut ids = repository
        .get_item_ids(&programmes(is_airing))
        .await
        .expect("query");
    ids.sort();

    assert_eq!(ids, expected);
}

#[rstest::rstest]
#[case::not_airing(false, vec!["Ended", "Upcoming"])]
#[case::airing(true, vec!["Airing"])]
#[tokio::test]
async fn the_genre_facet_follows_the_programmes_it_scopes_to(
    #[case] is_airing: bool,
    #[case] expected: Vec<&str>,
) {
    let db = seeded().await;
    let repository = item_repository_over(db.clone());

    let genres = repository
        .get_genres(&programmes(is_airing))
        .await
        .expect("genres");
    let mut names: Vec<String> = genres
        .items
        .into_iter()
        .filter_map(|row| row.item.name)
        .collect();
    names.sort();

    assert_eq!(names, expected);
}
