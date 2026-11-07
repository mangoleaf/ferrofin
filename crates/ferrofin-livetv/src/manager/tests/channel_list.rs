//! `GET /LiveTv/Channels` reads the guide only for the kinds it filters on.
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
    crate::guide_repository::test_support::channel_rows_plan(&db, query)
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
