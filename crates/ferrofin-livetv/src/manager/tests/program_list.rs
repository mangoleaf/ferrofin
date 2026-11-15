//! `GET`/`POST /LiveTv/Programs` and `GET /LiveTv/Programs/Recommended` list
//! the guide as upstream lists the stored `LiveTvProgram` items:
//! `BaseItemRepository.TranslateQuery` filters them by genre id
//! (`WhereReferencedItem`) and `ApplyOrder`/`OrderMapper` sort them (v12.4).

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy as By;
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

/// [`SortOrder::Ascending`], short for the cases below.
const ASC: SortOrder = SortOrder::Ascending;
/// [`SortOrder::Descending`], short for the cases below.
const DESC: SortOrder = SortOrder::Descending;

/// One airing of [`sorted_guide`]: `(channel, from, to, title, rating)`,
/// offsets in minutes from now.
type RatedAiring = (&'static str, i64, i64, &'static str, Option<&'static str>);

/// Airings that sort apart on every key the order tests send: two pairs that
/// start together; titles whose raw text, clean name and sort name order
/// three different ways; four lengths; and three ratings whose text order is
/// not their score order, beside an unrated one.
const SORTED: [RatedAiring; 4] = [
    ("one.tv", 60, 90, "Episode 10", Some("NC-17")),
    ("two.tv", 60, 110, "Episode 9", Some("TV-Y")),
    ("one.tv", 120, 180, "about", Some("PG-13")),
    ("two.tv", 120, 130, "Zoo", None),
];

/// [`SORTED`] as an XMLTV guide over the two [`M3U`] channels.
fn sorted_guide() -> String {
    use std::fmt::Write as _;
    let now = chrono::Utc::now();
    let mut xml = String::from(
        "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
         <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>",
    );
    for (channel, from, to, title, rating) in SORTED {
        let start = (now + chrono::Duration::minutes(from)).format("%Y%m%d%H%M%S");
        let stop = (now + chrono::Duration::minutes(to)).format("%Y%m%d%H%M%S");
        let rating = rating.map_or_else(String::new, |r| {
            format!("<rating system=\"MPAA\"><value>{r}</value></rating>")
        });
        let _ = write!(
            xml,
            "<programme start=\"{start} +0000\" stop=\"{stop} +0000\" channel=\"{channel}\">\
             <title>{title}</title>{rating}</programme>"
        );
    }
    xml.push_str("</tv>");
    xml
}

/// Upstream sorts the stored programme items through `ApplyOrder` and
/// `OrderMapper` (v12.4): ties on the start date break by sort name (the
/// default order jellyfin-web's suggestion rows use, cut by a limit); `Name`
/// is the clean name and `SortName` the number-padded sort name, with `Name`
/// after a leading `SortName`; `OfficialRating` is the parental score (an
/// unrated airing first); `Default` is dropped, leaving `SortName`; and every
/// other key sorts its column, `Runtime` by length and `DateCreated` (alike
/// for one refresh) into the sort-name tiebreak, as do the studio and
/// premiere-date keys over columns no listing fills. `SeriesDatePlayed` hands
/// the order to the played date of the series the row is, which no programme
/// is, then the sort name, dropping the other keys; and a direction that is not
/// `Ascending` sorts descending.
#[rstest::rstest]
#[case::no_key(&[], ["Episode 9", "Episode 10", "about", "Zoo"])]
#[case::start_date_desc(&[(By::StartDate, DESC)], ["about", "Zoo", "Episode 9", "Episode 10"])]
#[case::name(&[(By::Name, ASC)], ["about", "Episode 10", "Episode 9", "Zoo"])]
#[case::sort_name(&[(By::SortName, ASC)], ["about", "Episode 9", "Episode 10", "Zoo"])]
#[case::sort_name_desc(&[(By::SortName, DESC)], ["Zoo", "Episode 10", "Episode 9", "about"])]
#[case::default(&[(By::Default, ASC)], ["about", "Episode 9", "Episode 10", "Zoo"])]
#[case::rating(&[(By::OfficialRating, ASC)], ["Zoo", "Episode 9", "about", "Episode 10"])]
#[case::rating_desc(&[(By::OfficialRating, DESC)], ["Episode 10", "about", "Episode 9", "Zoo"])]
#[case::runtime(&[(By::Runtime, ASC)], ["Zoo", "Episode 10", "Episode 9", "about"])]
#[case::date_created(&[(By::DateCreated, ASC)], ["about", "Episode 9", "Episode 10", "Zoo"])]
#[case::studio(&[(By::Studio, DESC)], ["about", "Episode 9", "Episode 10", "Zoo"])]
#[case::premiere_date(&[(By::PremiereDate, ASC)], ["about", "Episode 9", "Episode 10", "Zoo"])]
#[case::series_date_played(&[(By::Runtime, ASC), (By::SeriesDatePlayed, DESC)], ["about", "Episode 9", "Episode 10", "Zoo"])]
#[case::unrecognized_order(&[(By::SortName, SortOrder::Unrecognized(7))], ["Zoo", "Episode 10", "Episode 9", "about"])]
#[tokio::test]
async fn the_guide_sorts_as_upstream_orders_the_programme_items(
    #[case] order_by: &[(By, SortOrder)],
    #[case] expected: [&str; 4],
) {
    let lab = item_lab(M3U, Some(sorted_guide())).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let query = InternalItemsQuery {
        order_by: order_by.to_vec(),
        ..InternalItemsQuery::default()
    };
    assert_eq!(listed(&lab, &query).await.0, expected);
}

/// What a viewer does to the "Zoo" airing before the guide is read.
#[derive(Debug, Clone, Copy)]
enum Mark {
    /// Starts playing it, which stamps `LastPlayedDate` and `PlayCount`.
    Play,
    /// Marks it played.
    Played,
    /// Makes it a favourite.
    Favourite,
}

/// A manager lab over [`sorted_guide`], refreshed, with a user and the user
/// data service that user's marks go through.
async fn viewer_lab() -> (
    ChannelItemLab,
    std::sync::Arc<ferrofin_db::entities::users::UserEntity>,
    ferrofin_core::FerrofinUserDataManager,
    tempfile::TempDir,
) {
    use std::sync::Arc;

    use ferrofin_traits::library::UserManager as _;

    let lab = item_lab(M3U, Some(sorted_guide())).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let users = ferrofin_core::user_manager::FerrofinUserManager::new(lab.mgr.db.clone());
    let user = Arc::new(users.create_user("viewer").await.expect("user"));
    let root = tempfile::tempdir().expect("root");
    let paths = Arc::new(ferrofin_core::FerrofinServerApplicationPaths::new(
        root.path().join("program"),
        root.path().join("log"),
        root.path().join("config"),
        root.path().join("cache"),
        root.path().join("web"),
    ));
    let config = ferrofin_core::FerrofinServerConfigurationManager::load(paths)
        .await
        .expect("configuration");
    let user_data =
        ferrofin_core::FerrofinUserDataManager::new(lab.mgr.db.clone(), Arc::new(config));
    (lab, user, user_data, root)
}

/// Each user-data key reads the requesting user's rows on the programme item
/// (`OrderMapper`, and `BuildIsPlayedFilter` for the played keys): the airing
/// the user played, marked played or made a favourite leads, the rest by sort
/// name. Without a user, `DatePlayed` reads every user's rows, as upstream's
/// does.
#[rstest::rstest]
#[case::date_played(By::DatePlayed, DESC, true, Mark::Play)]
#[case::date_played_by_anyone(By::DatePlayed, DESC, false, Mark::Play)]
#[case::play_count(By::PlayCount, DESC, true, Mark::Play)]
#[case::played(By::IsPlayed, DESC, true, Mark::Played)]
#[case::unplayed(By::IsUnplayed, ASC, true, Mark::Played)]
#[case::favourite(By::IsFavoriteOrLiked, DESC, true, Mark::Favourite)]
#[tokio::test]
async fn the_airing_a_user_marked_leads_each_user_data_order(
    #[case] by: By,
    #[case] order: SortOrder,
    #[case] as_the_user: bool,
    #[case] mark: Mark,
) {
    use ferrofin_traits::library::UserDataManager as _;

    let (lab, user, user_data, _root) = viewer_lab().await;
    let zoo = lab
        .mgr
        .get_programs(
            &InternalItemsQuery {
                name: Some("Zoo".to_owned()),
                ..InternalItemsQuery::default()
            },
            &DtoOptions::default(),
        )
        .await
        .expect("programs")
        .items[0]
        .id;
    let user_id = Uuid::parse_str(&user.id).expect("user id");
    match mark {
        Mark::Play => user_data
            .record_playback_start(user_id, zoo)
            .await
            .expect("play Zoo"),
        Mark::Played => {
            user_data
                .mark_played(user_id, zoo, None)
                .await
                .expect("mark Zoo played");
        }
        Mark::Favourite => {
            let favourite = ferrofin_model::dto::UpdateUserItemDataDto {
                is_favorite: Some(true),
                ..ferrofin_model::dto::UpdateUserItemDataDto::default()
            };
            user_data
                .save_user_data(user_id, zoo, &favourite)
                .await
                .expect("favourite Zoo");
        }
    }
    let query = InternalItemsQuery {
        user: as_the_user.then_some(user),
        order_by: vec![(by, order)],
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        listed(&lab, &query).await.0,
        ["Zoo", "about", "Episode 9", "Episode 10"]
    );
}

/// Every key a client can send, in either direction, with a user and
/// without, sorts the whole guide: each one's SQL runs.
#[tokio::test]
async fn every_sort_key_runs_with_and_without_a_user() {
    let (lab, user, _user_data, _root) = viewer_lab().await;
    let keys = [
        By::Default,
        By::AiredEpisodeOrder,
        By::Album,
        By::AlbumArtist,
        By::Artist,
        By::DateCreated,
        By::OfficialRating,
        By::DatePlayed,
        By::PremiereDate,
        By::StartDate,
        By::SortName,
        By::Name,
        By::Random,
        By::Runtime,
        By::CommunityRating,
        By::ProductionYear,
        By::PlayCount,
        By::CriticRating,
        By::IsFolder,
        By::IsUnplayed,
        By::IsPlayed,
        By::SeriesSortName,
        By::VideoBitRate,
        By::AirTime,
        By::Studio,
        By::IsFavoriteOrLiked,
        By::DateLastContentAdded,
        By::SeriesDatePlayed,
        By::ParentIndexNumber,
        By::IndexNumber,
        By::Unrecognized(99),
    ];
    for by in keys {
        for order in [ASC, DESC] {
            for viewer in [Some(std::sync::Arc::clone(&user)), None] {
                let query = InternalItemsQuery {
                    user: viewer,
                    order_by: vec![(by, order), (By::Name, ASC)],
                    ..InternalItemsQuery::default()
                };
                let (titles, total) = listed(&lab, &query).await;
                assert_eq!((titles.len(), total), (4, 4), "{by:?} {order:?}");
            }
        }
    }
}

/// The default order (jellyfin-web's suggestion rows send no key and a limit)
/// still walks the guide's start-date index and sorts only each start's ties
/// by the sort name, reading each airing's item by its key, so a limited page
/// stops early. A key over the item alone reads the item the same way.
#[tokio::test]
async fn the_guide_order_reads_each_airing_item_by_its_id() {
    let lab = item_lab(M3U, Some(sorted_guide())).await;
    let now = chrono::Utc::now();
    let plan_of = |order_by: Vec<(By, SortOrder)>| {
        let query = InternalItemsQuery {
            min_end_date: Some(now),
            order_by,
            limit: Some(20),
            ..InternalItemsQuery::default()
        };
        crate::guide_repository::test_support::plan(
            &lab.mgr.db,
            super::super::program_rows_query(&query, now, None),
        )
    };
    let item_by_id = "SEARCH item USING INDEX sqlite_autoindex_BaseItems_1 (Id=?) LEFT-JOIN";
    let default = plan_of(Vec::new()).await.expect("plan");
    for step in [
        "SCAN p USING INDEX FerrofinIX_LiveTvPrograms_StartDate",
        item_by_id,
        "USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    ] {
        assert!(default.iter().any(|d| d == step), "{step}: {default:?}");
    }
    let by_name = plan_of(vec![(By::SortName, ASC)]).await.expect("plan");
    assert!(by_name.iter().any(|d| d == item_by_id), "{by_name:?}");
}

/// Two airings on now and three later, over the categories the guide refresh
/// turns into flags (sports, kids, news) and one with none.
fn category_guide() -> String {
    super::guide_of(&[
        ("one.tv", -10, 50, "Match", "Sports"),
        ("two.tv", -10, 50, "Cartoons", "Kids"),
        ("one.tv", 60, 90, "Bulletin", "News"),
        ("two.tv", 60, 90, "Play", "Drama"),
        ("one.tv", 90, 120, "Late Game", "Sports"),
    ])
}

/// `IsSports`, `IsNews` and `IsKids` are tag sugar upstream
/// (`TranslateQuery.cs:224-258`): each `true` one adds its tag to the tags an
/// airing must carry ANY of, each `false` one to the tags it must carry none
/// of (`:665-677`). So two `true` flags list both kinds, and the total counts
/// the same set.
#[rstest::rstest]
#[case::sports_or_kids(Some(true), None, Some(true), &["Cartoons", "Match", "Late Game"])]
#[case::any_of_three(Some(true), Some(true), Some(true), &["Cartoons", "Match", "Bulletin", "Late Game"])]
#[case::sports_not_kids(Some(true), None, Some(false), &["Match", "Late Game"])]
#[case::neither(Some(false), None, Some(false), &["Bulletin", "Play"])]
#[case::news_only(None, Some(true), None, &["Bulletin"])]
#[tokio::test]
async fn the_category_flags_list_airings_with_any_true_one(
    #[case] is_sports: Option<bool>,
    #[case] is_news: Option<bool>,
    #[case] is_kids: Option<bool>,
    #[case] expected: &[&str],
) {
    let lab = item_lab(M3U, Some(category_guide())).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let query = InternalItemsQuery {
        is_sports,
        is_news,
        is_kids,
        limit: Some(10),
        enable_total_record_count: true,
        ..InternalItemsQuery::default()
    };
    let (titles, total) = listed(&lab, &query).await;
    assert_eq!(titles, expected);
    assert_eq!(usize::try_from(total).unwrap(), expected.len());
}

/// The Recommended airing branch reads its pool through the same filter, the
/// airing scope applying to the whole group: the sports airing that starts
/// later stays out.
#[tokio::test]
async fn the_recommended_airing_pool_takes_any_true_category() {
    let lab = item_lab(M3U, Some(category_guide())).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let query = InternalItemsQuery {
        is_airing: Some(true),
        is_sports: Some(true),
        is_kids: Some(true),
        ..InternalItemsQuery::default()
    };
    let (mut titles, total) = listed(&lab, &query).await;
    titles.sort();
    assert_eq!(
        (titles, total),
        (vec!["Cartoons".to_owned(), "Match".to_owned()], 2)
    );
}

/// The category flags leave the guide's plans where the column predicates
/// had them: one `true` flag (each jellyfin-web suggestion row) or every one
/// `false` still filters a scan of the guide and sorts what is left, as an
/// `IsMovie` predicate does, and two `true` ones, an `OR` group SQLite rates
/// as less selective, walk the start-date index as the unfiltered guide does.
/// None turns into a multi-index union, and each airing's item is still read
/// by its key.
#[rstest::rstest]
#[case::one_true(Some(true), None, None, "SCAN p")]
#[case::all_false(Some(false), Some(false), Some(false), "SCAN p")]
#[case::two_true(
    Some(true),
    None,
    Some(true),
    "SCAN p USING INDEX FerrofinIX_LiveTvPrograms_StartDate"
)]
#[tokio::test]
async fn the_category_flags_keep_the_guide_plans(
    #[case] is_sports: Option<bool>,
    #[case] is_news: Option<bool>,
    #[case] is_kids: Option<bool>,
    #[case] guide_step: &str,
) {
    let lab = item_lab(M3U, Some(category_guide())).await;
    let now = chrono::Utc::now();
    let query = InternalItemsQuery {
        is_sports,
        is_news,
        is_kids,
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
        guide_step,
        "SEARCH item USING INDEX sqlite_autoindex_BaseItems_1 (Id=?) LEFT-JOIN",
    ] {
        assert!(plan.iter().any(|d| d == step), "{step}: {plan:?}");
    }
    assert!(
        plan.iter().all(|d| !d.starts_with("MULTI-INDEX")),
        "{plan:?}"
    );
}
