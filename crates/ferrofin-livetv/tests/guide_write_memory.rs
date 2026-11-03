//! A guide refresh writes a listings provider's airings without holding them
//! all.
//!
//! Upstream's `GuideManager.RefreshChannelsInternal` asks the listings
//! provider for one channel's programmes at a time, and
//! `XmlTvListingsProvider.GetProgramsAsync` reads them from the cached file
//! for that channel alone (v12.4 GuideManager.cs:232-339,
//! XmlTvListingsProvider.cs:213-228); each channel's airings are stored and
//! dropped before the next channel's are read. This binary counts every heap
//! allocation with its own global allocator and checks that refreshing a
//! guide whose bound airings carry far more text than the limit below never
//! holds more than that limit at once, on the first refresh and on one that
//! replaces a stored guide. It holds one test, so no other test's allocations
//! are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_db::Database;
use ferrofin_livetv::fetch::{ReqwestFetcher, SourceFetcher};
use ferrofin_livetv::{FerrofinLiveTvManager, guide_repository};
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::stubs::LiveTvManager as _;

/// The heap bytes allocated right now.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The most heap bytes allocated at once since the last reset.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting what is live.
struct Counting;

impl Counting {
    fn grew(by: usize) {
        let now = LIVE.fetch_add(by, Ordering::SeqCst) + by;
        PEAK.fetch_max(now, Ordering::SeqCst);
    }

    fn shrank(by: usize) {
        LIVE.fetch_sub(by, Ordering::SeqCst);
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counters are
// atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            Self::grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            Self::grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        Self::shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                Self::shrank(layout.size() - new_size);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The tuner channels, each bound to the guide channel of the same id.
const CHANNELS: usize = 10;

/// Each channel's airings, half an hour apart from the refresh on: all of
/// them inside the default seven-day guide window.
const AIRINGS_PER_CHANNEL: usize = 200;

/// The length of every airing's description. Together the bound airings'
/// descriptions come to 32 MiB, eight times [`PEAK_LIMIT`].
const DESCRIPTION_BYTES: usize = 16 * 1024;

/// The most a refresh may hold at once. A write that collects the provider's
/// airings before it stores them holds every description; one that stores
/// them as they are read holds a statement's worth, plus each airing's id
/// and artwork for the refresh's clean-up.
const PEAK_LIMIT: usize = 4 * 1024 * 1024;

/// The playlist: [`CHANNELS`] channels whose `tvg-id` names a guide channel.
fn playlist() -> String {
    let mut m3u = String::from("#EXTM3U\n");
    for channel in 0..CHANNELS {
        let _ = writeln!(
            m3u,
            "#EXTINF:-1 tvg-id=\"c{channel}.tv\",Channel {channel}\nhttp://tuner/{channel}"
        );
    }
    m3u
}

/// The guide: the channels, then [`AIRINGS_PER_CHANNEL`] airings of each,
/// interleaved across the channels as XMLTV allows.
fn guide() -> String {
    let start = chrono::Utc::now();
    let description = "d".repeat(DESCRIPTION_BYTES);
    let mut xml = String::from("<tv>\n");
    for channel in 0..CHANNELS {
        let _ = writeln!(
            xml,
            "<channel id=\"c{channel}.tv\"><display-name>Channel {channel}</display-name></channel>"
        );
    }
    for airing in 0..AIRINGS_PER_CHANNEL {
        let from = start + chrono::Duration::minutes(30 * i64::try_from(airing).expect("small"));
        let to = from + chrono::Duration::minutes(30);
        for channel in 0..CHANNELS {
            let _ = writeln!(
                xml,
                "<programme start=\"{} +0000\" stop=\"{} +0000\" channel=\"c{channel}.tv\">\
                 <title>Airing {airing}</title><desc>{description}</desc></programme>",
                from.format("%Y%m%d%H%M%S"),
                to.format("%Y%m%d%H%M%S"),
            );
        }
    }
    xml.push_str("</tv>\n");
    xml
}

/// Refreshes the guide, answering the most it held at once.
async fn refresh(manager: &FerrofinLiveTvManager) -> usize {
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    manager.refresh_guide().await.expect("refresh");
    PEAK.load(Ordering::SeqCst).saturating_sub(before)
}

#[tokio::test]
async fn a_guide_refresh_writes_a_large_guide_without_holding_its_airings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m3u = dir.path().join("playlist.m3u");
    std::fs::write(&m3u, playlist()).expect("write playlist");
    let xmltv = dir.path().join("guide.xml");
    std::fs::write(&xmltv, guide()).expect("write guide");

    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrate");
    let manager = FerrofinLiveTvManager::new(
        db.clone(),
        Arc::new(ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
        "server".to_owned(),
        dir.path().join("cache"),
    );
    manager
        .save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some(m3u.to_string_lossy().into_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
    manager
        .save_listing_provider(
            ListingsProviderInfo {
                type_: Some("xmltv".to_owned()),
                path: Some(xmltv.to_string_lossy().into_owned()),
                ..ListingsProviderInfo::default()
            },
            false,
            false,
        )
        .await
        .expect("provider");

    let airings = CHANNELS * AIRINGS_PER_CHANNEL;
    let text = airings * DESCRIPTION_BYTES;
    // The first refresh stores the guide; the second replaces it.
    for pass in ["first", "second"] {
        let held = refresh(&manager).await;
        eprintln!("the {pass} refresh held {held} bytes at once");
        assert_eq!(
            guide_repository::all_program_ids(&db)
                .await
                .expect("ids")
                .len(),
            airings,
            "the {pass} refresh stores every bound airing"
        );
        assert!(
            held < PEAK_LIMIT,
            "the {pass} refresh of {airings} airings carrying {text} bytes of description \
             held {held} bytes at once (limit {PEAK_LIMIT})"
        );
    }
}
