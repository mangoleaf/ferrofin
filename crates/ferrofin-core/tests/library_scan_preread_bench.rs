//! Cost of the library scan's stored-row pre-read (opt-in,
//! `FERROFIN_PREREAD_BENCH=1`).
//!
//! CI never runs this: without the env var the test returns immediately. It
//! answers "what does reading every planned item's stored row cost?" for the
//! two shapes the scan could take — the whole plan at once, or one
//! `BATCH_BIND_CHUNK` window at a time (what `run_scan` does) — next to the
//! per-item existence check the pre-read replaced.
//! Heap is measured with a counting global allocator, so the numbers are the
//! rows' own allocations, not process RSS.
//!
//! ```text
//! FERROFIN_PREREAD_BENCH=1 FERROFIN_PREREAD_BENCH_ITEMS=20000 \
//!   cargo test --release -p ferrofin-core --test library_scan_preread_bench -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ferrofin_core::item_type_lookup::{ItemTypeLookup, stored_type_name};
use ferrofin_core::{FerrofinItemPersistenceService, FerrofinItemRepository};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

/// The system allocator plus a live-bytes counter and its high-water mark.
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

/// Runs `work`, returning its result, the wall time, and the heap high-water
/// mark above the live bytes at the start.
async fn measure<T, F: std::future::Future<Output = T>>(work: F) -> (T, f64, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let t = Instant::now();
    let out = work.await;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    (out, ms, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// Reads a `usize` knob from the environment, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::cast_precision_loss)]
async fn stored_rows_preread_cost() {
    if std::env::var("FERROFIN_PREREAD_BENCH").is_err() {
        eprintln!("library_scan_preread_bench: set FERROFIN_PREREAD_BENCH=1 to run");
        return;
    }
    let count = env_usize("FERROFIN_PREREAD_BENCH_ITEMS", 20_000);
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}",
        tmp.path().join("ferrofin.db").display()
    ))
    .await
    .unwrap();
    db.run_migrations().await.unwrap();

    // Episode rows shaped like a scanned library's: title, synopsis, path,
    // provider-owned text and a `Data` blob.
    let episode = stored_type_name(BaseItemKind::Episode).unwrap().to_owned();
    let ids: Vec<Uuid> = (0..count).map(|_| Uuid::new_v4()).collect();
    let rows: Vec<BaseItemEntity> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| BaseItemEntity {
            id: guid_to_db(*id),
            type_: episode.clone(),
            name: Some(format!("Episode title number {i}")),
            overview: Some("A synopsis of a typical length for a television episode. ".repeat(5)),
            path: Some(format!(
                "/media/tv/Some Series Name (2010)/Season 03/Some Series Name - S03E{i:05} - Episode title number {i}.mkv"
            )),
            genres: Some("Drama|Crime|Thriller".to_owned()),
            studios: Some("A Network".to_owned()),
            official_rating: Some("TV-14".to_owned()),
            data: Some(r#"{"VideoType":"VideoFile","SeriesStatus":"Ended"}"#.to_owned()),
            date_created: Some(chrono::Utc::now()),
            date_modified: Some(chrono::Utc::now()),
            ..Default::default()
        })
        .collect();
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    for chunk in rows.chunks(1000) {
        persistence.save_scanned_items(chunk).await.unwrap();
    }
    drop(rows);
    let repo = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));
    // Warm the page cache so every shape reads from the same state.
    repo.retrieve_items(&ids).await.unwrap();

    let (all, ms, heap) = measure(repo.retrieve_items(&ids)).await;
    let all = all.unwrap();
    assert_eq!(all.len(), count);
    drop(all);
    eprintln!(
        "whole plan at once   : {count} rows in {ms:.1} ms, peak heap {:.1} MiB",
        heap as f64 / 1_048_576.0
    );

    let ((), ms, heap) = measure(async {
        for window in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let rows = repo.retrieve_items(window).await.unwrap();
            assert_eq!(rows.len(), window.len());
        }
    })
    .await;
    eprintln!(
        "windowed ({:>4} ids) : {count} rows in {ms:.1} ms, peak heap {:.2} MiB",
        ferrofin_db::BATCH_BIND_CHUNK,
        heap as f64 / 1_048_576.0
    );

    let ((), ms, _) = measure(async {
        for id in &ids {
            assert!(repo.item_exists(*id).await.unwrap());
        }
    })
    .await;
    eprintln!("old item_exists loop : {count} queries in {ms:.1} ms");
}
