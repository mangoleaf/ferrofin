//! `GET`/`POST /LiveTv/Programs` and `GET /LiveTv/Programs/Recommended` list
//! the guide as upstream lists the stored `LiveTvProgram` items:
//! `BaseItemRepository.TranslateQuery` filters them by genre id
//! (`WhereReferencedItem`, v12.4).

use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::stubs::LiveTvManager as _;
use uuid::Uuid;

use super::{ChannelItemLab, M3U, item_lab, relative_guide};

/// A lab whose guide holds [`relative_guide`], refreshed, so every airing is
/// stored as an item with its genres.
async fn guide_lab() -> ChannelItemLab {
    let lab = item_lab(M3U, Some(relative_guide())).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    lab
}

/// The id of the by-name `Genre` item the guide refresh stored for `name`,
/// the id `GET /Genres` hands a client.
async fn genre_id(lab: &ChannelItemLab, name: &str) -> Uuid {
    let ids = lab
        .items
        .get_item_ids(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Genre],
            name: Some(name.to_owned()),
            ..InternalItemsQuery::default()
        })
        .await
        .expect("genre ids");
    assert_eq!(ids.len(), 1, "one {name} genre item: {ids:?}");
    ids[0]
}

/// The titles and the total a guide read returns.
async fn listed(lab: &ChannelItemLab, query: &InternalItemsQuery) -> (Vec<String>, i32) {
    let result = if query.is_airing == Some(true) {
        lab.mgr
            .get_recommended_programs(query, &DtoOptions::default())
            .await
    } else {
        lab.mgr.get_programs(query, &DtoOptions::default()).await
    }
    .expect("programs");
    let titles = result.items.into_iter().filter_map(|p| p.name).collect();
    (titles, result.total_record_count)
}

/// jellyfin-web's Live TV genre link opens `#/list?genreId=<id>&type=Programs`,
/// which sends `GenreIds` to both guide reads. Upstream keeps the airings whose
/// item carries a genre value cleaning to the referenced item's clean name,
/// any one of several ids; an id no item has keeps nothing. The total counts
/// under the same filter, and the Recommended airing branch filters its pool
/// too.
#[tokio::test]
async fn a_genre_id_lists_only_the_airings_of_that_genre() {
    let lab = guide_lab().await;
    let news = genre_id(&lab, "News").await;
    let comedy = genre_id(&lab, "Comedy").await;
    let drama = genre_id(&lab, "Drama").await;
    let by_genres = |genre_ids: Vec<Uuid>| InternalItemsQuery {
        genre_ids,
        ..InternalItemsQuery::default()
    };

    assert_eq!(
        listed(&lab, &by_genres(vec![news])).await,
        (vec!["Aired".to_owned(), "Two Now".to_owned()], 2)
    );
    assert_eq!(
        listed(&lab, &by_genres(vec![news, comedy])).await.0,
        ["Aired", "Two Now", "Later"]
    );
    assert_eq!(
        listed(&lab, &by_genres(vec![Uuid::from_u128(7)])).await,
        (Vec::new(), 0),
        "an id no item has"
    );
    let paged = InternalItemsQuery {
        limit: Some(1),
        enable_total_record_count: true,
        ..by_genres(vec![news])
    };
    assert_eq!(listed(&lab, &paged).await, (vec!["Aired".to_owned()], 2));

    let airing = |genre_ids: Vec<Uuid>| InternalItemsQuery {
        is_airing: Some(true),
        ..by_genres(genre_ids)
    };
    assert_eq!(listed(&lab, &airing(vec![news])).await.0, ["Two Now"]);
    assert_eq!(listed(&lab, &airing(vec![drama])).await.0, ["Now Playing"]);
}

/// The genre-id filter is a lookup per airing: the referenced items' clean
/// names are read once by their keys, each one's genre value by type and
/// clean value, and the airing's link to it by the link's own key, which
/// holds the airing's id. It never scans the item values or their links, and
/// the guide is still read off its start-date index.
#[tokio::test]
async fn the_genre_id_filter_reads_each_airing_values_by_its_id() {
    let lab = guide_lab().await;
    let now = chrono::Utc::now();
    let query = InternalItemsQuery {
        genre_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        min_end_date: Some(now),
        limit: Some(20),
        ..InternalItemsQuery::default()
    };
    let plan = crate::guide_repository::test_support::plan(
        &lab.mgr.db,
        super::super::program_rows_query(&query, now, None),
    )
    .await
    .expect("plan");
    for step in [
        "SCAN p USING INDEX FerrofinIX_LiveTvPrograms_StartDate",
        "SEARCH ref USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)",
        "SEARCH iv USING INDEX IX_ItemValues_Type_CleanValue (Type=? AND CleanValue=?)",
        "SEARCH ivm USING COVERING INDEX sqlite_autoindex_ItemValuesMap_1 (ItemValueId=? AND ItemId=?)",
    ] {
        assert!(plan.iter().any(|d| d == step), "{step}: {plan:?}");
    }
    assert!(
        plan.iter().any(|d| d.starts_with("LIST SUBQUERY")),
        "the clean names are an uncorrelated list: {plan:?}"
    );
    assert!(
        plan.iter()
            .all(|d| !d.starts_with("SCAN iv") && !d.starts_with("SCAN ref")),
        "{plan:?}"
    );
}
