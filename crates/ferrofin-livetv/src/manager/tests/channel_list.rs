//! `GET /LiveTv/Channels` reads the guide only for the kinds it filters on,
//! and lists only the channels the user's parental restrictions allow.
//!
//! The movie/series/kids kinds are derived from the airings, one correlated
//! pass over each channel's programmes per kind, and only the channel kind
//! filters read them. A lineup of thousands of IPTV channels must not pay for
//! them on every unfiltered list.

use std::collections::HashMap;

use ferrofin_db::Database;
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::options::DtoOptions;
use ferrofin_traits::stubs::{LiveTvChannelQuery, LiveTvManager as _};

use super::{CLASSIFIED_XMLTV, FakeFetcher, M3U, as_xmltv, dated, manager_with};

/// The plan step of one kind's pass over a channel's airings.
const KIND_PASS: &str =
    "SEARCH p USING INDEX FerrofinIX_LiveTvPrograms_ChannelId_StartDate (ChannelId=?)";

/// The lineup read's `EXPLAIN QUERY PLAN` details for `query`.
async fn plan(query: &LiveTvChannelQuery) -> Vec<String> {
    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrate");
    crate::guide_repository::test_support::channel_rows_plan(&db, query, None)
        .await
        .expect("plan")
}

/// Without a kind filter, the read is the channel table alone.
#[tokio::test]
async fn an_unfiltered_lineup_read_never_touches_the_guide() {
    let plan = plan(&LiveTvChannelQuery {
        is_news: Some(true),
        is_sports: Some(false),
        is_favorite: Some(true),
        enable_favorite_sorting: true,
        ..LiveTvChannelQuery::default()
    })
    .await;
    assert!(
        plan.iter()
            .all(|step| !step.contains("FerrofinLiveTvPrograms")
                && !step.contains("SUBQUERY")
                && !step.starts_with("SEARCH p")),
        "{plan:?}"
    );
}

/// Each kind filtered on, in either direction, adds exactly one pass, through
/// the `("ChannelId", "StartDate")` index.
#[rstest::rstest]
#[case::movie(Some(true), None, None, 1)]
#[case::not_series(None, Some(false), None, 1)]
#[case::kids(None, None, Some(true), 1)]
#[case::movie_and_not_kids(Some(true), None, Some(false), 2)]
#[case::every_kind(Some(false), Some(true), Some(true), 3)]
#[tokio::test]
async fn each_kind_filter_reads_its_kind_through_the_channel_index(
    #[case] is_movie: Option<bool>,
    #[case] is_series: Option<bool>,
    #[case] is_kids: Option<bool>,
    #[case] passes: usize,
) {
    let plan = plan(&LiveTvChannelQuery {
        is_movie,
        is_series,
        is_kids,
        ..LiveTvChannelQuery::default()
    })
    .await;
    assert_eq!(
        plan.iter().filter(|step| *step == KIND_PASS).count(),
        passes,
        "{plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.starts_with("SCAN p")),
        "{plan:?}"
    );
}

/// Several kind filters at once: each kind is derived, and a channel must
/// match every one. Every classified airing is on Channel One; Channel Two
/// airs none.
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
    let mut sources = HashMap::new();
    sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
    sources.insert("http://guide/xmltv.xml".to_owned(), dated(CLASSIFIED_XMLTV));
    let mgr = manager_with(FakeFetcher(sources)).await;
    mgr.save_tuner_host(TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some("http://tuner/playlist.m3u".to_owned()),
        ..TunerHostInfo::default()
    })
    .await
    .expect("tuner");
    mgr.save_listing_provider(
        as_xmltv(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        }),
        false,
        false,
    )
    .await
    .expect("provider");
    mgr.refresh_guide().await.expect("refresh");

    let query = LiveTvChannelQuery {
        is_movie,
        is_series,
        is_kids,
        ..LiveTvChannelQuery::default()
    };
    let names: Vec<String> = mgr
        .get_channels(&query, &DtoOptions::default())
        .await
        .expect("channels")
        .items
        .into_iter()
        .filter_map(|c| c.name)
        .collect();
    assert_eq!(names, expected);
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

    // A channel whose item is not stored is listed only to a user nothing
    // restricts.
    super::set_preference(mgr, &kid, PreferenceKind::AllowedTags, &[]).await;
    lab.persistence.delete_items(&[one]).await.expect("delete");
    let free = (vec!["Channel One".to_owned(), "Channel Two".to_owned()], 2);
    assert_eq!(lineup(mgr, &kid, None).await, free);
    assert_eq!(lineup(mgr, &limited, None).await, two_only);
}

/// A restricted lineup read, with every rule on (a maximum rating, a blocked
/// unrated kind, a blocked tag) and with a kind filter or without, probes each
/// channel's item by its key and keeps every step of the unrestricted plan:
/// the restriction is a correlated `EXISTS`, never a scan.
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
    ] {
        let restricted = channel_rows_plan(&mgr.db, &query, Some(&restrictions))
            .await
            .expect("plan");
        let unrestricted = channel_rows_plan(&mgr.db, &query, None)
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
