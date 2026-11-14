//! `GET /LiveTv/Channels` filters on what the guide refresh stored on the
//! channel items, lists only the channels the user's parental restrictions
//! allow, and sorts on the user's play state.
//!
//! The movie/series kinds are the channel item's columns and the kids, news
//! and sports kinds its tags, as upstream's `TranslateQuery` reads them. Each
//! kind filtered on probes the channel's item by its key, and no lineup read
//! touches the guide's airings: a lineup of thousands of IPTV channels pays
//! nothing for them.

use ferrofin_db::Database;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::options::DtoOptions;
use ferrofin_traits::stubs::{LiveTvChannelQuery, LiveTvManager as _};

use super::{CLASSIFIED_XMLTV, M3U, dated};

/// The plan step that keeps only the channels whose item is stored.
const STORED_PROBE: &str =
    "SEARCH bi EXISTS USING COVERING INDEX sqlite_autoindex_BaseItems_1 (Id=?)";
/// The plan step of a movie/series filter's probe of the channel item.
const ITEM_PROBE: &str = "SEARCH bi USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)";
/// The plan step of a kids/news/sports filter's probe of the channel's tag
/// link, after the tag's value is found by `("Type", "CleanValue")`.
const TAG_PROBE: &str =
    "SEARCH m USING COVERING INDEX sqlite_autoindex_ItemValuesMap_1 (ItemValueId=? AND ItemId=?)";

/// The lineup read's `EXPLAIN QUERY PLAN` details for `query`.
async fn plan(query: &LiveTvChannelQuery) -> Vec<String> {
    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrate");
    crate::guide_repository::test_support::channel_rows_plan(&db, query, None, true)
        .await
        .expect("plan")
}

/// Without a kind filter the read is the channel table and one probe of each
/// channel's item by its key (gl-1623), and no kind filter reads the guide's
/// airings.
#[tokio::test]
async fn no_lineup_read_touches_the_guide() {
    let unfiltered = plan(&LiveTvChannelQuery {
        is_favorite: Some(true),
        enable_favorite_sorting: true,
        ..LiveTvChannelQuery::default()
    })
    .await;
    let searches: Vec<&String> = unfiltered
        .iter()
        .filter(|step| step.contains("SEARCH") || step.contains("SUBQUERY"))
        .collect();
    assert_eq!(searches, [STORED_PROBE], "{unfiltered:?}");
    let every_kind = plan(&LiveTvChannelQuery {
        is_movie: Some(true),
        is_series: Some(false),
        is_kids: Some(true),
        is_news: Some(false),
        is_sports: Some(true),
        ..LiveTvChannelQuery::default()
    })
    .await;
    assert!(
        every_kind
            .iter()
            .all(|step| !step.contains("FerrofinLiveTvPrograms") && !step.contains(" p ")),
        "{every_kind:?}"
    );
}

/// Each column kind filtered on, in either direction, adds one probe of the
/// channel's stored item by its key, and the tag kinds one probe of its tag
/// links for those asked for and one for those asked against. Nothing is
/// scanned but the channel table.
#[rstest::rstest]
#[case::movie(LiveTvChannelQuery { is_movie: Some(true), ..LiveTvChannelQuery::default() }, 1, 0)]
#[case::not_series(LiveTvChannelQuery { is_series: Some(false), ..LiveTvChannelQuery::default() }, 1, 0)]
#[case::kids(LiveTvChannelQuery { is_kids: Some(true), ..LiveTvChannelQuery::default() }, 0, 1)]
#[case::not_news(LiveTvChannelQuery { is_news: Some(false), ..LiveTvChannelQuery::default() }, 0, 1)]
#[case::kids_or_sports(
    LiveTvChannelQuery { is_kids: Some(true), is_sports: Some(true), ..LiveTvChannelQuery::default() },
    0,
    1
)]
#[case::movie_and_not_kids(
    LiveTvChannelQuery { is_movie: Some(true), is_kids: Some(false), ..LiveTvChannelQuery::default() },
    1,
    1
)]
#[case::every_kind(
    LiveTvChannelQuery {
        is_movie: Some(false),
        is_series: Some(true),
        is_kids: Some(true),
        is_news: Some(true),
        is_sports: Some(false),
        ..LiveTvChannelQuery::default()
    },
    2,
    2
)]
#[tokio::test]
async fn each_kind_filter_probes_the_channel_item_by_key(
    #[case] query: LiveTvChannelQuery,
    #[case] item_probes: usize,
    #[case] tag_probes: usize,
) {
    let plan = plan(&query).await;
    let count = |probe: &str| plan.iter().filter(|step| *step == probe).count();
    assert_eq!(
        (count(ITEM_PROBE), count(TAG_PROBE)),
        (item_probes, tag_probes),
        "{plan:?}"
    );
    let scans: Vec<&String> = plan.iter().filter(|s| s.starts_with("SCAN")).collect();
    assert_eq!(scans, ["SCAN FerrofinLiveTvChannels"], "{plan:?}");
}

/// Several kind filters at once: a channel must match every one. Every
/// classified airing is on Channel One; Channel Two airs none.
#[rstest::rstest]
#[case::series_and_kids(None, Some(true), Some(true), &["Channel One"])]
#[case::movie_but_not_kids(Some(true), None, Some(false), &[])]
#[case::neither_movie_nor_kids(Some(false), None, Some(false), &["Channel Two"])]
#[tokio::test]
async fn several_kind_filters_must_all_match(
    #[case] is_movie: Option<bool>,
    #[case] is_series: Option<bool>,
    #[case] is_kids: Option<bool>,
    #[case] expected: &[&str],
) {
    let lab = super::item_lab(M3U, Some(dated(CLASSIFIED_XMLTV))).await;
    lab.mgr.refresh_guide().await.expect("refresh");

    let query = LiveTvChannelQuery {
        is_movie,
        is_series,
        is_kids,
        ..LiveTvChannelQuery::default()
    };
    assert_eq!(names(&lab.mgr, query).await, expected);
}

/// The names `GET /LiveTv/Channels` lists for `query`.
async fn names(
    mgr: &crate::manager::FerrofinLiveTvManager,
    query: LiveTvChannelQuery,
) -> Vec<String> {
    mgr.get_channels(&query, &DtoOptions::default())
        .await
        .expect("channels")
        .items
        .into_iter()
        .filter_map(|c| c.name)
        .collect()
}

/// gl-1620: upstream's `isKids` reads the channel's "Kids" tag, which the
/// refresh adds and never removes (`GuideManager.cs:306-316`), so a channel
/// stays a kids channel after its kids airing leaves the guide. A tag an
/// editor gives a channel, in any case, answers `isSports` and `isNews` the
/// same way. Kinds asked for match a channel carrying any of their tags
/// (`TranslateQuery` gathers them into one tag list).
#[tokio::test]
async fn kind_filters_read_the_stored_channel_items() {
    use ferrofin_db::enums::ItemValueType;
    let lab = super::item_lab(M3U, Some(dated(CLASSIFIED_XMLTV))).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let kids = || LiveTvChannelQuery {
        is_kids: Some(true),
        ..LiveTvChannelQuery::default()
    };
    assert_eq!(names(&lab.mgr, kids()).await, ["Channel One"]);

    // The next guide airs nothing on either channel.
    lab.sources
        .0
        .lock()
        .expect("sources")
        .insert("http://guide/xmltv.xml".to_owned(), dated("<tv></tv>"));
    let providers = lab.mgr.get_listing_providers().await.expect("providers");
    let provider = providers[0].id.as_deref().expect("provider id");
    tokio::fs::remove_file(lab.mgr.xmltv.cache_file_for_test(provider))
        .await
        .expect("drop the cached guide");
    lab.mgr.refresh_guide().await.expect("second refresh");
    assert_eq!(names(&lab.mgr, kids()).await, ["Channel One"]);
    let series = LiveTvChannelQuery {
        is_series: Some(true),
        ..LiveTvChannelQuery::default()
    };
    assert_eq!(names(&lab.mgr, series).await, Vec::<String>::new());

    let [_, two] = super::channel_ids(&lab.mgr).await[..] else {
        panic!("two channels");
    };
    let tag = vec![(i32::from(ItemValueType::Tags), "SPORTS".to_owned())];
    lab.persistence
        .save_item_values_for_items(&[(two, tag)])
        .await
        .expect("tag");
    for (want, expected) in [(true, "Channel Two"), (false, "Channel One")] {
        let sports = LiveTvChannelQuery {
            is_sports: Some(want),
            ..LiveTvChannelQuery::default()
        };
        assert_eq!(names(&lab.mgr, sports).await, [expected], "isSports={want}");
    }
    let news = LiveTvChannelQuery {
        is_news: Some(true),
        ..LiveTvChannelQuery::default()
    };
    assert_eq!(names(&lab.mgr, news).await, Vec::<String>::new());
    let either = |want| LiveTvChannelQuery {
        is_kids: Some(want),
        is_sports: Some(want),
        ..LiveTvChannelQuery::default()
    };
    assert_eq!(
        names(&lab.mgr, either(true)).await,
        ["Channel One", "Channel Two"]
    );
    assert_eq!(names(&lab.mgr, either(false)).await, Vec::<String>::new());
    for (kids, expected) in [(true, "Channel One"), (false, "Channel Two")] {
        let mixed = LiveTvChannelQuery {
            is_kids: Some(kids),
            is_sports: Some(!kids),
            ..LiveTvChannelQuery::default()
        };
        assert_eq!(names(&lab.mgr, mixed).await, [expected], "isKids={kids}");
    }
}

/// The names `GET /LiveTv/Channels` lists to `user` with `limit`, and the
/// total it reports.
async fn lineup(
    mgr: &crate::manager::FerrofinLiveTvManager,
    user: &std::sync::Arc<ferrofin_db::entities::users::UserEntity>,
    limit: Option<i32>,
) -> (Vec<String>, i32) {
    let page = mgr
        .get_channels(
            &LiveTvChannelQuery {
                user: Some(std::sync::Arc::clone(user)),
                limit,
                ..LiveTvChannelQuery::default()
            },
            &DtoOptions::default(),
        )
        .await
        .expect("channels");
    let names = page.items.into_iter().filter_map(|c| c.name).collect();
    (names, page.total_record_count)
}

/// Upstream's `GetInternalChannels` lists the channel items through
/// `new InternalItemsQuery(user)`, so the user's blocked unrated kinds,
/// maximum rating and blocked and allowed tags filter the lineup before its
/// page is cut: the first row and the total are the visible ones.
#[tokio::test]
async fn the_lineup_hides_what_a_users_parental_restrictions_hide() {
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::enums::{ItemValueType, PreferenceKind};
    let (lab, kid) = super::rated_guide_lab().await;
    let mgr = &lab.mgr;
    let both = vec!["Channel One".to_owned(), "Channel Two".to_owned()];
    assert_eq!(lineup(mgr, &kid, None).await, (both, 2));
    let [one, two] = super::channel_ids(mgr).await[..] else {
        panic!("two channels");
    };

    // No channel carries a rating.
    let unrated = ["LiveTvChannel"];
    super::set_preference(mgr, &kid, PreferenceKind::BlockUnratedItems, &unrated).await;
    assert_eq!(lineup(mgr, &kid, None).await, (vec![], 0));
    super::set_preference(mgr, &kid, PreferenceKind::BlockUnratedItems, &[]).await;

    // A rating the metadata editor gave Channel One, above a TV-PG limit.
    let rated = BaseItemEntity {
        official_rating: Some("TV-MA".to_owned()),
        inherited_parental_rating_value: Some(super::us_score("TV-MA")),
        ..super::channel_item(&lab.items, one).await
    };
    lab.persistence.save_items(&[rated]).await.expect("rated");
    let limited = super::tv_pg(&kid);
    let two_only = (vec!["Channel Two".to_owned()], 1);
    assert_eq!(lineup(mgr, &limited, Some(1)).await, two_only);

    // A tag on Channel Two: blocked, it hides the channel; allowed, it keeps
    // only the channels that carry it.
    let tag = vec![(i32::from(ItemValueType::Tags), "Sports".to_owned())];
    let values = [(two, tag)];
    lab.persistence
        .save_item_values_for_items(&values)
        .await
        .expect("tag");
    super::set_preference(mgr, &kid, PreferenceKind::BlockedTags, &["Sports"]).await;
    let one_only = (vec!["Channel One".to_owned()], 1);
    assert_eq!(lineup(mgr, &kid, None).await, one_only);
    super::set_preference(mgr, &kid, PreferenceKind::BlockedTags, &[]).await;
    super::set_preference(mgr, &kid, PreferenceKind::AllowedTags, &["Sports"]).await;
    assert_eq!(lineup(mgr, &kid, None).await, two_only);

    // gl-1623: a channel whose item is not stored is listed to no one, as
    // upstream lists only the items.
    super::set_preference(mgr, &kid, PreferenceKind::AllowedTags, &[]).await;
    lab.persistence.delete_items(&[one]).await.expect("delete");
    assert_eq!(lineup(mgr, &kid, None).await, two_only);
    assert_eq!(lineup(mgr, &limited, None).await, two_only);
    // Nor does a negated kind filter, which a missing item would satisfy.
    let not_kids = LiveTvChannelQuery {
        is_kids: Some(false),
        ..LiveTvChannelQuery::default()
    };
    assert_eq!(names(mgr, not_kids).await, ["Channel Two"]);
}

/// What the user does to Channel Two before the list is read.
#[derive(Debug, Clone, Copy)]
enum Mark {
    /// Starts playing it.
    Play,
    /// Marks it played.
    Played,
    /// Makes it a favourite.
    Favourite,
}

/// jellyfin-web's guide in its "Recently watched" channel order asks for
/// `SortBy=DatePlayed&SortOrder=Descending`, favourites first (`guide.js`).
/// Upstream's `OrderMapper` sorts it, and the other play-state keys, on the
/// user data written on the channel item: playback stamps `LastPlayedDate`
/// and `PlayCount` (`SessionManager.OnPlaybackStart`; a channel has no played
/// status, so only a mark-played sets `Played`). A channel never played has
/// no row and sorts as `NULL`, or unplayed. Each key loads the user data
/// itself, with favourite sorting off too.
#[rstest::rstest]
#[case::recently_watched(ItemSortBy::DatePlayed, SortOrder::Descending, true, Mark::Play)]
#[case::date_played(ItemSortBy::DatePlayed, SortOrder::Descending, false, Mark::Play)]
#[case::most_played(ItemSortBy::PlayCount, SortOrder::Descending, false, Mark::Play)]
#[case::played_first(ItemSortBy::IsPlayed, SortOrder::Descending, false, Mark::Played)]
#[case::unplayed_last(ItemSortBy::IsUnplayed, SortOrder::Ascending, false, Mark::Played)]
#[case::favourite_key(
    ItemSortBy::IsFavoriteOrLiked,
    SortOrder::Descending,
    false,
    Mark::Favourite
)]
#[tokio::test]
async fn a_channel_the_user_played_leads_on_each_play_state_key(
    #[case] sort_by: ItemSortBy,
    #[case] sort_order: SortOrder,
    #[case] enable_favorite_sorting: bool,
    #[case] mark: Mark,
) {
    use std::sync::Arc;

    use ferrofin_traits::library::{UserDataManager as _, UserManager as _};

    let lab = super::channel_item_lab().await;
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
    let [_, two] = super::channel_ids(&lab.mgr).await[..] else {
        panic!("two channels");
    };
    let user_id = uuid::Uuid::parse_str(&user.id).expect("user id");
    match mark {
        Mark::Play => user_data
            .record_playback_start(user_id, two)
            .await
            .expect("play Channel Two"),
        Mark::Played => {
            user_data
                .mark_played(user_id, two, None)
                .await
                .expect("mark Channel Two played");
        }
        Mark::Favourite => {
            let favourite = ferrofin_model::dto::UpdateUserItemDataDto {
                is_favorite: Some(true),
                ..ferrofin_model::dto::UpdateUserItemDataDto::default()
            };
            user_data
                .save_user_data(user_id, two, &favourite)
                .await
                .expect("favourite Channel Two");
        }
    }

    let names: Vec<String> = lab
        .mgr
        .get_channels(
            &LiveTvChannelQuery {
                user: Some(user),
                sort_by: vec![sort_by],
                sort_order: Some(sort_order),
                enable_favorite_sorting,
                ..LiveTvChannelQuery::default()
            },
            &DtoOptions::default(),
        )
        .await
        .expect("channels")
        .items
        .into_iter()
        .filter_map(|c| c.name)
        .collect();
    assert_eq!(names, ["Channel Two", "Channel One"]);
}

/// A restricted lineup read, with every rule on (a maximum rating, a blocked
/// unrated kind, a blocked tag) and with a kind filter or without, probes each
/// channel's item by its key and keeps every step of the plan that lists the
/// whole lineup: the restriction is a correlated `EXISTS`, never a scan, and
/// stands in for the stored-item probe.
#[tokio::test]
async fn a_restricted_lineup_read_probes_the_channel_item_by_key() {
    use crate::guide_repository::test_support::channel_rows_plan;
    use ferrofin_db::enums::PreferenceKind;
    use ferrofin_traits::options::InternalItemsQuery;
    let (lab, kid) = super::rated_guide_lab().await;
    let mgr = &lab.mgr;
    let unrated = ["LiveTvChannel"];
    super::set_preference(mgr, &kid, PreferenceKind::BlockUnratedItems, &unrated).await;
    super::set_preference(mgr, &kid, PreferenceKind::BlockedTags, &["News"]).await;
    let user = InternalItemsQuery {
        user: Some(super::tv_pg(&kid)),
        ..InternalItemsQuery::default()
    };
    let restrictions = mgr.user_restrictions(&user).await.expect("resolve");
    let restrictions = restrictions.expect("a restricted user");
    for query in [
        LiveTvChannelQuery::default(),
        LiveTvChannelQuery {
            is_kids: Some(true),
            ..LiveTvChannelQuery::default()
        },
        LiveTvChannelQuery {
            is_movie: Some(true),
            is_kids: Some(false),
            ..LiveTvChannelQuery::default()
        },
    ] {
        let restricted = channel_rows_plan(&mgr.db, &query, Some(&restrictions), true)
            .await
            .expect("plan");
        let unrestricted = channel_rows_plan(&mgr.db, &query, None, false)
            .await
            .expect("plan");
        for step in &unrestricted {
            assert!(restricted.contains(step), "{step} kept: {restricted:?}");
        }
        assert!(
            restricted
                .iter()
                .any(|d| d == "SEARCH bi EXISTS USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"),
            "{restricted:?}"
        );
        // The channel table's own scan is the only one.
        assert!(
            restricted
                .iter()
                .filter(|d| d.starts_with("SCAN"))
                .all(|d| unrestricted.contains(d)),
            "{restricted:?}"
        );
    }
}
