//! A stop reaches inside the guide refresh.
//!
//! `GuideManager.RefreshGuide(progress, cancellationToken)` (v12.4
//! src/Jellyfin.LiveTv/Guide/GuideManager.cs:100-171) checks its token before
//! each service, each channel item and each channel's programmes, and hands
//! it to the tuner and listings fetches. These tests stop a refresh at a
//! tuner and at a listings provider: in a fetch that never answers, and in
//! one that does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::LiveTvManager as _;
use ferrofin_traits::tasks::ScanCancel;

use super::{M3U, XMLTV, as_xmltv, dated, manager_with_fetcher};
use crate::fetch::SourceFetcher;
use crate::manager::FerrofinLiveTvManager;

const TUNER_A: &str = "http://tuner-a/playlist.m3u";
const TUNER_B: &str = "http://tuner-b/playlist.m3u";
const GUIDE: &str = "http://guide/xmltv.xml";
/// Tuner B's playlist: one channel no other tuner lists.
const M3U_B: &str = "#EXTM3U\n\
    #EXTINF:-1 tvg-id=\"three.tv\" tvg-chno=\"3\",Channel Three\nhttp://tuner-b/three\n";

/// Far longer than a stopped refresh takes, far shorter than a hung test.
const WAIT: Duration = Duration::from_secs(10);

/// What a [`StopFetcher`] does at its marked URL.
enum At {
    /// It never answers: a slow source.
    Hang,
    /// It asks for the stop and answers: a stop during a fetch that finishes.
    Stop(ScanCancel),
}

/// A [`SourceFetcher`] over both tuners' playlists and the guide that records
/// what it was asked for, in order, and does `then` when asked for `at`.
struct StopFetcher {
    at: &'static str,
    then: At,
    asked: Mutex<Vec<String>>,
}

impl StopFetcher {
    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("asked").clone()
    }
}

#[async_trait::async_trait]
impl SourceFetcher for StopFetcher {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        self.asked.lock().expect("asked").push(url.to_owned());
        if url == self.at {
            match &self.then {
                At::Hang => std::future::pending::<()>().await,
                At::Stop(stop) => stop.cancel(),
            }
        }
        let bodies = HashMap::from([
            (TUNER_A, M3U.to_owned()),
            (TUNER_B, M3U_B.to_owned()),
            (GUIDE, dated(XMLTV)),
        ]);
        bodies
            .get(url)
            .cloned()
            .ok_or_else(|| ServiceError::Backend(format!("no {url}")))
    }
}

/// A manager over two M3U tuners and one XMLTV provider, all served by a
/// [`StopFetcher`] that does `then` at `at`.
async fn lab(at: &'static str, then: At) -> (FerrofinLiveTvManager, Arc<StopFetcher>) {
    let fetcher = Arc::new(StopFetcher {
        at,
        then,
        asked: Mutex::new(Vec::new()),
    });
    let mgr = manager_with_fetcher(Arc::clone(&fetcher) as Arc<dyn SourceFetcher>).await;
    // Stored directly: `save_tuner_host` opens the playlist first
    // (`M3UTunerHost.Validate`), and one of these sources may never answer.
    for (id, url) in [("a", TUNER_A), ("b", TUNER_B)] {
        let tuner = TunerHostInfo {
            id: Some(id.to_owned()),
            type_: Some("m3u".to_owned()),
            url: Some(url.to_owned()),
            ..TunerHostInfo::default()
        };
        mgr.db
            .upsert_live_tv_tuner_host(
                id,
                url,
                "m3u",
                &serde_json::to_string(&tuner).expect("json"),
            )
            .await
            .expect("tuner");
    }
    mgr.save_listing_provider(
        as_xmltv(ListingsProviderInfo {
            path: Some(GUIDE.to_owned()),
            ..ListingsProviderInfo::default()
        }),
        false,
        false,
    )
    .await
    .expect("listings provider");
    (mgr, fetcher)
}

async fn stored_channels(mgr: &FerrofinLiveTvManager) -> usize {
    crate::guide_repository::all_channel_ids(&mgr.db)
        .await
        .expect("channel ids")
        .len()
}

/// A stop while a tuner or a listings provider is still being read ends that
/// fetch and the refresh: upstream's fetch throws for its token, and the
/// refresh throws at its next check. Nothing later is asked, nothing is
/// written, and the refresh says it was stopped.
#[rstest::rstest]
#[case::a_tuner(TUNER_A, &[TUNER_A])]
#[case::a_listings_provider(GUIDE, &[TUNER_A, TUNER_B, GUIDE])]
#[tokio::test]
async fn a_stop_ends_the_fetch_in_flight_and_the_refresh(
    #[case] slow: &'static str,
    #[case] asked: &[&str],
) {
    let (mgr, fetcher) = lab(slow, At::Hang).await;
    let stop = ScanCancel::new();
    let stopper = async {
        while !fetcher.asked().iter().any(|url| url == slow) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop.cancel();
    };
    let (finished, ()) = tokio::time::timeout(WAIT, async {
        tokio::join!(mgr.refresh_guide_until(&stop), stopper)
    })
    .await
    .expect("the stop ended the refresh");
    assert!(!finished.expect("refresh"), "a stopped refresh says so");
    assert_eq!(fetcher.asked(), asked, "nothing later was asked");
    assert_eq!(stored_channels(&mgr).await, 0, "nothing was written");
}

/// A stop during a fetch that still answers ends the refresh at its next
/// check: before the next tuner, and before the write once the listings are
/// in. The next refresh writes the guide.
#[rstest::rstest]
#[case::before_the_next_tuner(TUNER_A, &[TUNER_A])]
#[case::before_the_write(GUIDE, &[TUNER_A, TUNER_B, GUIDE])]
#[tokio::test]
async fn a_stop_ends_the_refresh_at_its_next_check(
    #[case] stopped_at: &'static str,
    #[case] asked: &[&str],
) {
    let stop = ScanCancel::new();
    let (mgr, fetcher) = lab(stopped_at, At::Stop(stop.clone())).await;
    let finished = tokio::time::timeout(WAIT, mgr.refresh_guide_until(&stop))
        .await
        .expect("the refresh returned")
        .expect("refresh");
    assert!(!finished, "a stopped refresh says so");
    assert_eq!(fetcher.asked(), asked, "nothing later was asked");
    assert_eq!(stored_channels(&mgr).await, 0, "nothing was written");

    let finished = mgr
        .refresh_guide_until(&ScanCancel::new())
        .await
        .expect("next refresh");
    assert!(finished, "a refresh nobody stopped runs to its end");
    assert_eq!(stored_channels(&mgr).await, 3, "both tuners' channels");
}
