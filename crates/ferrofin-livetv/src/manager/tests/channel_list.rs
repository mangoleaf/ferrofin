//! `GET /LiveTv/Channels` filters on what the guide refresh stored on the
//! channel items, lists only the channels the user's parental restrictions
//! allow, and orders them over those items, the user's play state included.
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

/// The plan step that joins each channel's stored item, which the list is
/// ordered on and which keeps only the channels whose item is stored.
const STORED_PROBE: &str = "SEARCH item USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)";
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
    crate::guide_repository::test_support::channel_rows_plan(&db, query, None, true, None)
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
/// no row and sorts as `NULL`, or unplayed. Each key reads the user data in
/// the listing's own statement, with favourite sorting off too.
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
/// whole lineup: the restriction is a correlated `EXISTS`, never a scan.
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
        let restricted = channel_rows_plan(&mgr.db, &query, Some(&restrictions), true, None)
            .await
            .expect("plan");
        let unrestricted = channel_rows_plan(&mgr.db, &query, None, true, None)
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

/// With no user (an API key), `SortBy=DatePlayed` orders the lineup by the
/// newest play any user made on each channel: upstream's `OrderMapper` reads
/// all of `UserData` when the query has no user (`OrderMapper.cs:32-48`).
/// One viewer plays Channel One, then another plays Channel Two, so Channel
/// Two leads descending, against the name order a missing key would leave.
#[tokio::test]
async fn without_a_user_date_played_orders_by_anyones_newest_play() {
    use std::sync::Arc;

    use ferrofin_traits::library::{UserDataManager as _, UserManager as _};

    let lab = super::channel_item_lab().await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let users = ferrofin_core::user_manager::FerrofinUserManager::new(lab.mgr.db.clone());
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
    let [one, two] = super::channel_ids(&lab.mgr).await[..] else {
        panic!("two channels");
    };
    for (name, channel) in [("first", one), ("second", two)] {
        let user = users.create_user(name).await.expect("user");
        let user_id = uuid::Uuid::parse_str(&user.id).expect("user id");
        user_data
            .record_playback_start(user_id, channel)
            .await
            .expect("play");
    }

    // Favourite sorting sorts nothing without a user, so `DatePlayed` decides
    // behind it. The other user-data keys are `SortName` without a user, as
    // `/Items` and `/LiveTv/Programs` read them (upstream reads the missing
    // user's id and fails), in the requested direction.
    let (two_first, one_first) = (
        ["Channel Two", "Channel One"],
        ["Channel One", "Channel Two"],
    );
    for (sort_by, enable_favorite_sorting, sort_order, expected) in [
        (
            vec![ItemSortBy::DatePlayed],
            false,
            SortOrder::Descending,
            two_first,
        ),
        (
            vec![ItemSortBy::DatePlayed],
            false,
            SortOrder::Ascending,
            one_first,
        ),
        (
            vec![ItemSortBy::PlayCount, ItemSortBy::DatePlayed],
            false,
            SortOrder::Descending,
            two_first,
        ),
        (
            vec![ItemSortBy::PlayCount, ItemSortBy::DatePlayed],
            false,
            SortOrder::Ascending,
            one_first,
        ),
        (
            vec![ItemSortBy::IsPlayed, ItemSortBy::DatePlayed],
            false,
            SortOrder::Descending,
            two_first,
        ),
        (
            vec![ItemSortBy::DatePlayed],
            true,
            SortOrder::Descending,
            two_first,
        ),
    ] {
        let names: Vec<String> = lab
            .mgr
            .get_channels(
                &LiveTvChannelQuery {
                    sort_by: sort_by.clone(),
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
        assert_eq!(names, expected, "{sort_by:?} {sort_order:?}");
    }
}

/// The no-user `DatePlayed` key seeks each channel's rows by item, never
/// scanning every user's `UserData`.
#[tokio::test]
async fn the_no_user_last_played_key_seeks_user_data_by_item() {
    let plan = plan(&LiveTvChannelQuery {
        sort_by: vec![ItemSortBy::DatePlayed],
        ..LiveTvChannelQuery::default()
    })
    .await;
    assert!(
        plan.iter()
            .any(|d| d.starts_with("SEARCH ud USING ") && d.ends_with("(ItemId=?)")),
        "{plan:?}"
    );
    assert!(!plan.iter().any(|d| d.starts_with("SCAN ud")), "{plan:?}");
}

/// The lineup [`sorted`] lists: `ALPHA` (`2.0`), `Charlie` and `alpha` share
/// the number 2, `bravo` is 10 and `Delta` has none, so their stored sort
/// names are `0000000002.00000-ALPHA`, `…-Charlie`, `…-alpha`,
/// `0000000010.00000-bravo` and `-Delta`.
const SORT_M3U: &str = "#EXTM3U\n\
    #EXTINF:-1 tvg-id=\"e\" tvg-chno=\"2.0\",ALPHA\nhttp://tuner/e\n\
    #EXTINF:-1 tvg-id=\"a\" tvg-chno=\"10\",bravo\nhttp://tuner/a\n\
    #EXTINF:-1 tvg-id=\"b\" tvg-chno=\"2\",Charlie\nhttp://tuner/b\n\
    #EXTINF:-1 tvg-id=\"c\" tvg-chno=\"2\",alpha\nhttp://tuner/c\n\
    #EXTINF:-1 tvg-id=\"d\",Delta\nhttp://tuner/d\n";

/// [`SORT_M3U`]'s channels stored by a guide refresh, a viewer, and that
/// viewer's user data on them: `alpha` is a favourite, marked played and
/// watched three times, last on 1 November; `bravo` was watched once, last
/// on 3 November, and never marked played (as playback leaves a channel);
/// `Charlie` has a row with no play; `ALPHA` and `Delta` have none. Each
/// item's `DateCreated` is set as the table below reads it.
async fn sort_lab() -> (
    super::ChannelItemLab,
    std::sync::Arc<ferrofin_db::entities::users::UserEntity>,
    std::collections::HashMap<String, uuid::Uuid>,
) {
    use ferrofin_traits::library::UserManager as _;

    let lab = super::item_lab(SORT_M3U, None).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    let users = ferrofin_core::user_manager::FerrofinUserManager::new(lab.mgr.db.clone());
    let user = std::sync::Arc::new(users.create_user("viewer").await.expect("user"));
    let ids: std::collections::HashMap<String, uuid::Uuid> = lab
        .mgr
        .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
        .await
        .expect("channels")
        .items
        .into_iter()
        .map(|c| (c.name.expect("name"), c.id))
        .collect();
    for (name, day) in [
        ("ALPHA", 2),
        ("bravo", 3),
        ("Charlie", 1),
        ("alpha", 2),
        ("Delta", 1),
    ] {
        let mut item = lab
            .items
            .retrieve_item(ids[name])
            .await
            .expect("read")
            .expect("stored item");
        item.date_created = chrono::NaiveDate::from_ymd_opt(2026, 1, day)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|at| at.and_utc());
        lab.persistence.save_items(&[item]).await.expect("save");
    }
    for (name, is_favorite, plays) in [
        (
            "alpha",
            true,
            (3, true, Some("2026-11-01 08:00:00.0000000")),
        ),
        (
            "bravo",
            false,
            (1, false, Some("2026-11-03 08:00:00.0000000")),
        ),
        ("Charlie", false, (0, false, None)),
    ] {
        crate::guide_repository::test_support::seed_user_data(
            &lab.mgr.db,
            &ferrofin_db::store::guid_to_db(ids[name]),
            &user.id,
            &crate::projection::ChannelUserDataRow {
                is_favorite,
                rating: None,
            },
            plays,
        )
        .await
        .expect("user data");
    }
    (lab, user, ids)
}

/// The names `GET /LiveTv/Channels` lists for the viewer of [`sort_lab`].
async fn sorted(
    lab: &super::ChannelItemLab,
    user: &std::sync::Arc<ferrofin_db::entities::users::UserEntity>,
    sort_by: &[ItemSortBy],
    sort_order: Option<SortOrder>,
    enable_favorite_sorting: bool,
) -> Vec<String> {
    names(
        &lab.mgr,
        LiveTvChannelQuery {
            user: Some(std::sync::Arc::clone(user)),
            sort_by: sort_by.to_vec(),
            sort_order,
            enable_favorite_sorting,
            ..LiveTvChannelQuery::default()
        },
    )
    .await
}

const ASC: Option<SortOrder> = Some(SortOrder::Ascending);
const DESC: Option<SortOrder> = Some(SortOrder::Descending);

/// gl-1681: the lineup is ordered over the stored channel items as
/// `ApplyOrder` orders them, every key as `OrderMapper` maps it and each in
/// the requested order, favourites first when asked, then `SortName`
/// ascending. `SortName` compares byte-wise as SQLite's `BINARY` does, so an
/// unnumbered channel (`-Delta`) leads and `ALPHA` < `Charlie` < `alpha`; a
/// leading `SortName` is followed by `Name`. `Name` is `CleanName`. A
/// channel without a play is `NULL` for `DatePlayed` and `PlayCount` (first
/// ascending, last descending) and unplayed.
#[rstest::rstest]
#[case::sort_name(&[], None, false, ["Delta", "ALPHA", "Charlie", "alpha", "bravo"])]
// `ApplyOrder` drops `Default`: the appended ascending `SortName` is left.
#[case::default_dropped(&[ItemSortBy::Default], DESC, false, ["Delta", "ALPHA", "Charlie", "alpha", "bravo"])]
#[case::sort_name_desc(&[ItemSortBy::SortName], DESC, false, ["bravo", "alpha", "Charlie", "ALPHA", "Delta"])]
// `ALPHA` and `alpha` share the clean name: `SortName` breaks the tie.
#[case::name(&[ItemSortBy::Name], None, false, ["ALPHA", "alpha", "bravo", "Charlie", "Delta"])]
#[case::name_desc(&[ItemSortBy::Name], DESC, false, ["Delta", "Charlie", "bravo", "ALPHA", "alpha"])]
#[case::date_created(&[ItemSortBy::DateCreated], None, false, ["Delta", "Charlie", "ALPHA", "alpha", "bravo"])]
#[case::date_created_desc(&[ItemSortBy::DateCreated], DESC, false, ["bravo", "ALPHA", "alpha", "Delta", "Charlie"])]
#[case::favourites_first(&[], None, true, ["alpha", "Delta", "ALPHA", "Charlie", "bravo"])]
#[case::favourites_then_name(&[ItemSortBy::Name], DESC, true, ["alpha", "Delta", "Charlie", "bravo", "ALPHA"])]
#[case::favourite_key(&[ItemSortBy::IsFavoriteOrLiked], DESC, false, ["alpha", "Delta", "ALPHA", "Charlie", "bravo"])]
#[case::date_played_desc(&[ItemSortBy::DatePlayed], DESC, false, ["bravo", "alpha", "Delta", "ALPHA", "Charlie"])]
#[case::date_played(&[ItemSortBy::DatePlayed], ASC, false, ["Delta", "ALPHA", "Charlie", "alpha", "bravo"])]
// The guide's "Recently watched" order.
#[case::recently_watched(&[ItemSortBy::DatePlayed], DESC, true, ["alpha", "bravo", "Delta", "ALPHA", "Charlie"])]
#[case::play_count_desc(&[ItemSortBy::PlayCount], DESC, false, ["alpha", "bravo", "Charlie", "Delta", "ALPHA"])]
#[case::play_count(&[ItemSortBy::PlayCount], ASC, false, ["Delta", "ALPHA", "Charlie", "bravo", "alpha"])]
// Only the played flag counts: `bravo`'s play leaves it unplayed.
#[case::played_desc(&[ItemSortBy::IsPlayed], DESC, false, ["alpha", "Delta", "ALPHA", "Charlie", "bravo"])]
#[case::played(&[ItemSortBy::IsPlayed], ASC, false, ["Delta", "ALPHA", "Charlie", "bravo", "alpha"])]
#[case::unplayed_desc(&[ItemSortBy::IsUnplayed], DESC, false, ["Delta", "ALPHA", "Charlie", "bravo", "alpha"])]
#[case::unplayed(&[ItemSortBy::IsUnplayed], ASC, false, ["alpha", "Delta", "ALPHA", "Charlie", "bravo"])]
// `AirTime` and the keys `OrderMapper` has no arm for are `SortName`, with no
// `Name` after it: only a `SortName` key takes one.
#[case::air_time(&[ItemSortBy::AirTime], DESC, false, ["bravo", "alpha", "Charlie", "ALPHA", "Delta"])]
#[case::unknown_key(&[ItemSortBy::Unrecognized(99)], DESC, false, ["bravo", "alpha", "Charlie", "ALPHA", "Delta"])]
#[case::aired_episode_order(&[ItemSortBy::AiredEpisodeOrder], None, false, ["Delta", "ALPHA", "Charlie", "alpha", "bravo"])]
// A key over a column no channel item fills ties.
#[case::runtime(&[ItemSortBy::Runtime], DESC, false, ["Delta", "ALPHA", "Charlie", "alpha", "bravo"])]
// `SeriesDatePlayed` leaves `SortName` ascending alone: favourites and the
// other keys are dropped.
#[case::series_date_played(
    &[ItemSortBy::PlayCount, ItemSortBy::SeriesDatePlayed],
    DESC,
    true,
    ["Delta", "ALPHA", "Charlie", "alpha", "bravo"]
)]
#[tokio::test]
async fn the_lineup_sorts_on_each_requested_key_over_the_channel_items(
    #[case] sort_by: &[ItemSortBy],
    #[case] sort_order: Option<SortOrder>,
    #[case] enable_favorite_sorting: bool,
    #[case] expected: [&str; 5],
) {
    let (lab, user, _) = sort_lab().await;
    assert_eq!(
        sorted(&lab, &user, sort_by, sort_order, enable_favorite_sorting).await,
        expected
    );
}

/// `Random` below the favourites: `alpha` leads, and the rest is a fresh
/// order on each call.
#[tokio::test]
async fn a_random_lineup_is_a_fresh_order_each_time() {
    let (lab, user, _) = sort_lab().await;
    let mut draws = std::collections::HashSet::new();
    for _ in 0..64 {
        draws.insert(sorted(&lab, &user, &[ItemSortBy::Random], None, true).await);
    }
    assert!(draws.len() > 1, "{draws:?}");
    for draw in &draws {
        assert_eq!(draw[0], "alpha", "{draw:?}");
        let mut names = draw.clone();
        names.sort();
        assert_eq!(names, ["ALPHA", "Charlie", "Delta", "alpha", "bravo"]);
    }
}

/// What the metadata editor sets on a channel item moves the channel, as
/// upstream sorts the stored item: a forced sort name replaces the derived
/// one (the save keeps the stored `SortName` the edit wrote), and a
/// community rating sorts, `NULL` last descending (gl-1681).
#[tokio::test]
async fn an_edited_channel_item_moves_in_the_list() {
    let (lab, user, ids) = sort_lab().await;
    for (name, edit) in [
        (
            "bravo",
            (|item: &mut ferrofin_db::entities::base_items::BaseItemEntity| {
                item.forced_sort_name = Some("!".to_owned());
                item.sort_name = Some("!".to_owned());
            }) as fn(&mut ferrofin_db::entities::base_items::BaseItemEntity),
        ),
        ("Charlie", |item| item.community_rating = Some(9.0)),
    ] {
        let mut item = lab
            .items
            .retrieve_item(ids[name])
            .await
            .expect("read")
            .expect("stored item");
        edit(&mut item);
        lab.persistence.save_items(&[item]).await.expect("save");
    }
    assert_eq!(
        sorted(&lab, &user, &[], None, false).await,
        ["bravo", "Delta", "ALPHA", "Charlie", "alpha"]
    );
    assert_eq!(
        sorted(&lab, &user, &[ItemSortBy::CommunityRating], DESC, false).await,
        ["Charlie", "bravo", "Delta", "ALPHA", "alpha"]
    );
}

/// A leading `SortName` is followed by `Name` in its direction, as
/// `ApplyOrder`'s `ThenBy(e => e.Name)` is. Two channels sharing a forced sort
/// name fall to their names byte-wise: `ALPHA` before `alpha` ascending, and
/// `alpha` first descending, where the lineup's scan order would keep `ALPHA`
/// first (gl-1681).
#[tokio::test]
async fn a_shared_sort_name_falls_to_the_name() {
    let (lab, user, ids) = sort_lab().await;
    for name in ["ALPHA", "alpha"] {
        let mut item = lab
            .items
            .retrieve_item(ids[name])
            .await
            .expect("read")
            .expect("stored item");
        item.forced_sort_name = Some("same".to_owned());
        item.sort_name = Some("same".to_owned());
        lab.persistence.save_items(&[item]).await.expect("save");
    }
    assert_eq!(
        sorted(&lab, &user, &[ItemSortBy::SortName], ASC, false).await,
        ["Delta", "Charlie", "bravo", "ALPHA", "alpha"]
    );
    assert_eq!(
        sorted(&lab, &user, &[ItemSortBy::SortName], DESC, false).await,
        ["alpha", "ALPHA", "bravo", "Charlie", "Delta"]
    );
}

/// The guide's own query, a user with favourites first (`guide.js`), seeks
/// each channel's user data by item and user: no scan but the channel table's.
#[tokio::test]
async fn favourite_sorting_seeks_each_channels_user_data() {
    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrate");
    let plan = crate::guide_repository::test_support::channel_rows_plan(
        &db,
        &LiveTvChannelQuery {
            enable_favorite_sorting: true,
            sort_by: vec![ItemSortBy::DatePlayed],
            sort_order: Some(SortOrder::Descending),
            ..LiveTvChannelQuery::default()
        },
        None,
        true,
        Some("00000000000000000000000000000001"),
    )
    .await
    .expect("plan");
    assert!(
        plan.iter()
            .any(|d| d.starts_with("SEARCH ud USING ") && d.ends_with("(ItemId=? AND UserId=?)")),
        "{plan:?}"
    );
    let scans: Vec<&String> = plan.iter().filter(|s| s.starts_with("SCAN")).collect();
    assert_eq!(scans, ["SCAN FerrofinLiveTvChannels"], "{plan:?}");
}

/// `SortBy=Name` sorts on `CleanName`, as `OrderMapper` maps it: punctuation is
/// a space and diacritics go, so `a-b` (`a b`) leads `a c`, and `Éclair` sorts
/// as `eclair`, where the lower-cased names put `a c` first and `éclair` last.
/// The order Jellyfin 12.4 lists for these names (gl-1681).
#[tokio::test]
async fn name_sorts_on_the_clean_name() {
    let m3u = "#EXTM3U\n\
        #EXTINF:-1 tvg-id=\"e\",Éclair\nhttp://tuner/e\n\
        #EXTINF:-1 tvg-id=\"s\",a c\nhttp://tuner/s\n\
        #EXTINF:-1 tvg-id=\"d\",a-b\nhttp://tuner/d\n\
        #EXTINF:-1 tvg-id=\"z\",zulu\nhttp://tuner/z\n\
        #EXTINF:-1 tvg-id=\"a\",abc\nhttp://tuner/a\n";
    let lab = super::item_lab(m3u, None).await;
    lab.mgr.refresh_guide().await.expect("refresh");
    assert_eq!(
        names(
            &lab.mgr,
            LiveTvChannelQuery {
                sort_by: vec![ItemSortBy::Name],
                ..LiveTvChannelQuery::default()
            },
        )
        .await,
        ["a-b", "a c", "abc", "Éclair", "zulu"]
    );
}
