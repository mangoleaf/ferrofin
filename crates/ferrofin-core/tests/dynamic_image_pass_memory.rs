//! The closing dynamic-image pass of a library scan holds one window of rows,
//! not a whole kind (gl-1482).
//!
//! A home-video or photo library makes a `Folder` or `PhotoAlbum` row of every
//! directory, so tens of thousands of them are realistic. The pass awaits the
//! scan's priority lane and draws each row in turn, which takes minutes, and
//! reading a kind's rows whole held them all for that time (about 27 MB at
//! 20,000 directories). This binary counts the live heap with its own global
//! allocator and checks that the pass's high-water mark does not grow with
//! the number of rows: over eight `BATCH_BIND_CHUNK` windows of folders it
//! stays within one window of where it was over two. SQLite's own pages are
//! not Rust allocations, so the in-memory database is not counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::FerrofinItemPersistenceService;
use ferrofin_core::FerrofinItemRepository;
use ferrofin_core::dynamic_images::DynamicImageProviders;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_db::{BATCH_BIND_CHUNK, Database};
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_traits::persistence::ItemPersistenceService as _;

/// The system allocator plus a live-bytes counter and its high-water mark.
/// The counters are process-wide, so this binary holds one test: a second,
/// run alongside it, would move them.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` unchanged; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The directories of the first pass, then of the second: two windows, then
/// eight.
const FEW: usize = 2 * BATCH_BIND_CHUNK;
const MANY: usize = 8 * BATCH_BIND_CHUNK;

/// One window of rows, inline. A pass over the whole kind at once grows by
/// one of these for every 500 directories more.
const WINDOW: usize = BATCH_BIND_CHUNK * std::mem::size_of::<BaseItemEntity>();

/// Seeds the folder rows numbered `range`.
async fn seed(persistence: &FerrofinItemPersistenceService, range: std::ops::Range<usize>) {
    let folders: Vec<BaseItemEntity> = range
        .map(|n| BaseItemEntity {
            id: guid_to_db(uuid::Uuid::new_v4()),
            type_: "MediaBrowser.Controller.Entities.Folder".to_owned(),
            name: Some(format!("Holiday {n:05}")),
            path: Some(format!("/media/home videos/Holiday {n:05}")),
            is_folder: true,
            ..Default::default()
        })
        .collect();
    persistence
        .save_items(&folders)
        .await
        .expect("seed folders");
}

/// Runs the pass and returns how many rows it examined and how far its
/// high-water mark rose above the live heap it started at.
async fn pass_peak(providers: &DynamicImageProviders) -> (usize, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let report = providers.refresh_all().await.expect("pass");
    let added = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    (report.examined, added)
}

#[tokio::test(flavor = "current_thread")]
async fn the_closing_pass_holds_a_window_of_folders_not_all_of_them() {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let tmp = tempfile::tempdir().expect("tempdir");
    let items = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let providers = DynamicImageProviders::new(
        items,
        persistence.clone(),
        Arc::new(ImageProcessor::new(
            Arc::new(ImageCrateEncoder::new()),
            tmp.path().join("cache"),
        )),
        tmp.path().join("metadata").join("library"),
    );

    seed(&persistence, 0..FEW).await;
    let (examined, few) = pass_peak(&providers).await;
    assert_eq!(examined, FEW);
    seed(&persistence, FEW..MANY).await;
    let (examined, many) = pass_peak(&providers).await;
    assert_eq!(examined, MANY);

    assert!(
        many <= few + WINDOW,
        "the pass peaked {few} bytes above where it started over {FEW} \
         folders and {many} over {MANY}: it holds the kind's rows, not a window"
    );
}
