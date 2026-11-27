//! A running guide refresh reports its progress.
//!
//! `GuideManager.RefreshGuide` hands `RefreshChannelsInternal` its progress
//! (v12.4 src/Jellyfin.LiveTv/Guide/GuideManager.cs:126, 179-342): 10 before
//! the service's channels are read, 15 once every channel item is stored,
//! `(85 * percent) + 15` after each channel's programmes, then 100. The
//! first two tests run the "Refresh Guide" task over the real manager and
//! look at what its run had reported when the refresh read its sources and
//! each channel's stored airings; the last asks the manager itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferrofin_core::scheduled_tasks::live_tv::RefreshGuideTask;
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::LiveTvManager as _;
use ferrofin_traits::tasks::{ScheduledTask as _, TaskProgress};

use super::guide_mirror::{AIRINGS, ChannelReads, guide};
use super::{
    FakeFetcher, M3U, XMLTV, as_xmltv, dated, guide_day, item_lab_reading_through, manager_with,
    manager_with_fetcher,
};
use crate::fetch::SourceFetcher;

const TUNER: &str = "http://tuner/playlist.m3u";
const GUIDE: &str = "http://guide/xmltv.xml";

/// What a run had reported at each look, in order.
type Reported = Arc<Mutex<Vec<Option<f64>>>>;

/// A [`SourceFetcher`] over one playlist and its guide that notes what `run`
/// had reported each time it was asked.
struct ReportedAtFetch {
    run: TaskProgress,
    seen: Reported,
}

#[async_trait::async_trait]
impl SourceFetcher for ReportedAtFetch {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        self.seen.lock().expect("seen").push(self.run.reported());
        match url {
            TUNER => Ok(M3U.to_owned()),
            GUIDE => Ok(dated(XMLTV)),
            _ => Err(ServiceError::Backend(format!("no {url}"))),
        }
    }
}

/// The run reads 10 while the tuner and the listings are fetched, as
/// upstream's does from `progress.Report(10)` until its channels are stored.
#[tokio::test]
async fn the_sources_are_read_at_10() {
    let run = TaskProgress::default();
    let seen = Reported::default();
    let mgr = manager_with_fetcher(Arc::new(ReportedAtFetch {
        run: run.clone(),
        seen: Arc::clone(&seen),
    }))
    .await;
    mgr.save_tuner_host(TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some(TUNER.to_owned()),
        ..TunerHostInfo::default()
    })
    .await
    .expect("tuner");
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
    // Saving the tuner read its playlist: only the run's own fetches count.
    let before = seen.lock().expect("seen").len();

    RefreshGuideTask::new(Arc::new(mgr))
        .execute(&run)
        .await
        .expect("run");

    assert_eq!(
        seen.lock().expect("seen")[before..],
        [Some(10.0), Some(10.0)],
        "the playlist, then the guide"
    );
}

/// Each channel's programmes move the run on by its share of the 85 points
/// after 15: with two channels it reads 15 while the first channel's airings
/// are mirrored and 57.5 while the second's are.
#[tokio::test]
async fn each_channels_programmes_move_the_run_on_from_15() {
    let run = TaskProgress::default();
    let seen = Reported::default();
    let lab = item_lab_reading_through(M3U, Some(guide(&guide_day(0), &AIRINGS)), |inner| {
        Arc::new(ChannelReads {
            inner,
            reads: Mutex::new(Vec::new()),
            at_read: Box::new({
                let (run, seen) = (run.clone(), Arc::clone(&seen));
                move || seen.lock().expect("seen").push(run.reported())
            }),
        })
    })
    .await;
    // The first import has no stored airing to read back, so nothing is seen.
    lab.mgr.refresh_guide().await.expect("first refresh");
    assert!(seen.lock().expect("seen").is_empty());

    RefreshGuideTask::new(Arc::new(lab.mgr.clone()))
        .execute(&run)
        .await
        .expect("run");

    assert_eq!(
        *seen.lock().expect("seen"),
        [Some(15.0), Some(57.5)],
        "one read of stored airings per channel"
    );
}

/// The refresh ends on 100 by itself, `RefreshChannelsInternal`'s closing
/// report, when no channel's programmes moved it there. The manager is asked
/// directly, so the 100 is not the task's own closing one.
#[tokio::test]
async fn a_refresh_with_no_channel_ends_on_100() {
    let run = TaskProgress::default();
    // No tuner and no listings provider: nothing is fetched.
    let mgr = manager_with(FakeFetcher(HashMap::new())).await;
    let finished = mgr.refresh_guide_until(&run).await.expect("refresh");
    assert!(finished, "nobody stopped it");
    assert_eq!(run.reported(), Some(100.0));
}
