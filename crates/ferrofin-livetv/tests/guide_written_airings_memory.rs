//! A guide refresh keeps one map of the airings it wrote, not two.
//!
//! The refresh's clean-up and the item mirror need every airing a listings
//! provider wrote, with its artwork: the provider's write hands back that map,
//! and the refresh merges each provider's into one. Upstream's
//! `GuideManager.RefreshGuide` keeps the ids alone
//! (`newProgramIdList.AddRange(programIds)`, v12.4 GuideManager.cs:114-131).
//! Extending an empty map reserves room for everything the source holds, so
//! merging the first provider's map that way allocated a second table the size
//! of the first while the first was still alive. This binary counts the heap
//! blocks big enough to hold every written airing with its own global
//! allocator, and checks that a refresh never has two of them at once. It
//! holds one test, so no other test's allocations are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_db::Database;
use ferrofin_livetv::fetch::{ReqwestFetcher, SourceFetcher};
use ferrofin_livetv::projection::ProgramImages;
use ferrofin_livetv::{FerrofinLiveTvManager, guide_repository};
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::stubs::LiveTvManager as _;
use uuid::Uuid;

/// The size from which a heap block counts as big; 0 counts nothing.
static BIG: AtomicUsize = AtomicUsize::new(0);

/// The big blocks allocated right now.
static BIG_LIVE: AtomicUsize = AtomicUsize::new(0);

/// The most big blocks allocated at once since the last reset.
static BIG_PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting the big blocks that are live.
struct Counting;

impl Counting {
    fn is_big(size: usize) -> bool {
        let big = BIG.load(Ordering::SeqCst);
        big > 0 && size >= big
    }

    fn allocated(size: usize) {
        if Self::is_big(size) {
            let now = BIG_LIVE.fetch_add(1, Ordering::SeqCst) + 1;
            BIG_PEAK.fetch_max(now, Ordering::SeqCst);
        }
    }

    fn freed(size: usize) {
        if Self::is_big(size) {
            // A block allocated before counting began is not in the count.
            let _ = BIG_LIVE.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        }
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counters are
// atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            Self::allocated(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            Self::allocated(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        Self::freed(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            Self::freed(layout.size());
            Self::allocated(new_size);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The tuner channels, each bound to the guide channel of the same id.
const CHANNELS: usize = 40;

/// Each channel's airings, five minutes apart from the refresh on: all of
/// them inside the default seven-day guide window.
const AIRINGS_PER_CHANNEL: usize = 700;

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

/// The guide: the channels, then [`AIRINGS_PER_CHANNEL`] short airings of
/// each, so that the map of written airings is the refresh's biggest holder.
fn guide() -> String {
    let start = chrono::Utc::now();
    let mut xml = String::from("<tv>\n");
    for channel in 0..CHANNELS {
        let _ = writeln!(
            xml,
            "<channel id=\"c{channel}.tv\"><display-name>Channel {channel}</display-name></channel>"
        );
    }
    for airing in 0..AIRINGS_PER_CHANNEL {
        let from = start + chrono::Duration::minutes(5 * i64::try_from(airing).expect("small"));
        let to = from + chrono::Duration::minutes(5);
        for channel in 0..CHANNELS {
            let _ = writeln!(
                xml,
                "<programme start=\"{} +0000\" stop=\"{} +0000\" channel=\"c{channel}.tv\">\
                 <title>Airing {airing}</title></programme>",
                from.format("%Y%m%d%H%M%S"),
                to.format("%Y%m%d%H%M%S"),
            );
        }
    }
    xml.push_str("</tv>\n");
    xml
}

#[tokio::test]
async fn a_guide_refresh_never_holds_two_maps_of_its_written_airings() {
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
    // Any hash table holding every written airing is at least this big; the
    // refresh's other holders (the stored ids it cleans against, a statement's
    // worth of rows) are far smaller.
    let table = airings * size_of::<(Uuid, ProgramImages)>();
    BIG_LIVE.store(0, Ordering::SeqCst);
    BIG_PEAK.store(0, Ordering::SeqCst);
    BIG.store(table, Ordering::SeqCst);
    manager.refresh_guide().await.expect("refresh");
    BIG.store(0, Ordering::SeqCst);
    let held = BIG_PEAK.load(Ordering::SeqCst);

    assert_eq!(
        guide_repository::all_program_ids(&db)
            .await
            .expect("ids")
            .len(),
        airings,
        "the refresh stores every bound airing"
    );
    assert_eq!(
        held, 1,
        "a refresh of {airings} airings held {held} blocks of at least {table} bytes at once; \
         the map of written airings should be the only one"
    );
}
