//! Where a library re-scan's heap goes (opt-in, `FERROFIN_SCAN_MEMORY_BENCH=1`).
//!
//! CI never runs this: without the env var the test returns immediately. It
//! answers "how much of a re-scan's heap is the resident plan, and how much
//! the prereads?" on a synthetic movie and TV library shaped like a scanned
//! one: every item has an NFO with its provider ids, and movies and series
//! carry a poster and a backdrop, episodes a thumb, all real PNGs the first
//! scan probes, so the re-scan pre-reads complete image rows.
//!
//! Heap is measured with a counting global allocator (the rows' own
//! allocations, not process RSS; SQLite's page cache uses its own allocator
//! and is not in it). A tracing layer reads the live bytes at the scan's own
//! markers: `library scan planned` (the plan is resident, the prereads have
//! not run) and each `scanning item` (the first one follows the up-front
//! prereads and the first stored-row window's). As a reference, the
//! provider-id and image-probe maps are also built for every stored row at
//! once, which is what reading them for the whole plan up front would hold.
//!
//! The corpus' videos are empty files and no ffprobe is wired, so each
//! re-scan saves the videos again: the walk allocates more than a quiet
//! re-scan of probed files would.
//!
//! ```text
//! FERROFIN_SCAN_MEMORY_BENCH=1 \
//!   cargo test -p ferrofin-core --test library_scan_memory_bench -- --nocapture
//! ```
//!
//! `FERROFIN_SCAN_MEMORY_MOVIES` (default 3000), `FERROFIN_SCAN_MEMORY_SERIES`
//! (250) and `FERROFIN_SCAN_MEMORY_EPISODES` (30 per series, in 3 seasons)
//! size the corpus; the defaults follow the benchmark corpus' video half.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::{Layer, filter::Targets};

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

/// The live bytes and the high-water mark at `library scan planned`.
static PLANNED_LIVE: AtomicUsize = AtomicUsize::new(0);
static PLANNED_PEAK: AtomicUsize = AtomicUsize::new(0);
/// The `items` the planned line reported.
static PLANNED_ITEMS: AtomicUsize = AtomicUsize::new(0);
/// The live bytes at the first `scanning item`, and the most at any.
static FIRST_ITEM_LIVE: AtomicUsize = AtomicUsize::new(0);
static MAX_ITEM_LIVE: AtomicUsize = AtomicUsize::new(0);
static ITEMS_SEEN: AtomicUsize = AtomicUsize::new(0);

/// A message buffer on the stack, so reading an event's message allocates
/// nothing the counter would see.
struct StackText {
    buf: [u8; 48],
    len: usize,
}

impl std::fmt::Write for StackText {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = self.buf.len() - self.len;
        let take = s.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// What one event says: its message and an `items` field.
struct Marker {
    text: StackText,
    items: u64,
}

impl tracing::field::Visit for Marker {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.text, "{value:?}");
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "items" {
            self.items = value;
        }
    }
}

/// Reads the live heap at the scan's phase markers.
struct PhaseLayer;

impl<S: tracing::Subscriber> Layer<S> for PhaseLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut marker = Marker {
            text: StackText {
                buf: [0; 48],
                len: 0,
            },
            items: 0,
        };
        event.record(&mut marker);
        let live = LIVE.load(Ordering::Relaxed);
        match &marker.text.buf[..marker.text.len] {
            b"library scan planned" => {
                PLANNED_LIVE.store(live, Ordering::Relaxed);
                PLANNED_PEAK.store(PEAK.load(Ordering::Relaxed), Ordering::Relaxed);
                PLANNED_ITEMS.store(
                    usize::try_from(marker.items).unwrap_or(usize::MAX),
                    Ordering::Relaxed,
                );
            }
            b"scanning item" => {
                if ITEMS_SEEN.fetch_add(1, Ordering::Relaxed) == 0 {
                    FIRST_ITEM_LIVE.store(live, Ordering::Relaxed);
                }
                MAX_ITEM_LIVE.fetch_max(live, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Reads a `usize` knob from the environment, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A small real PNG the scan's image probe decodes.
fn write_png(path: &Path, width: u32, height: u32, seed: usize) {
    #[allow(clippy::cast_possible_truncation)]
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([seed as u8, (x * 7) as u8, (y * 5) as u8])
    });
    image.save(path).expect("png");
}

/// A synopsis of a typical length.
const PLOT: &str = "A synopsis of a typical length for a film or an episode, \
    long enough to stand for what a scraped NFO carries, with a second \
    sentence that names a place, a person and a reason to keep watching.";

/// Writes the corpus: `movies` movie folders and `series` series of
/// `episodes` episodes each, in three seasons.
fn write_corpus(root: &Path, movies: usize, series: usize, episodes: usize) {
    let movie_root = root.join("movies");
    for i in 0..movies {
        let year = 1950 + i % 70;
        let dir = movie_root.join(format!("Movie Title {i:05} ({year})"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("Movie Title {i:05} ({year}).mkv")), b"").expect("mkv");
        std::fs::write(
            dir.join("movie.nfo"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<movie>\n  \
                 <title>Movie Title {i:05}</title>\n  <plot>{PLOT}</plot>\n  \
                 <year>{year}</year>\n  <genre>Drama</genre>\n  <genre>Thriller</genre>\n  \
                 <studio>A Studio</studio>\n  \
                 <uniqueid type=\"tmdb\" default=\"true\">{}</uniqueid>\n  \
                 <uniqueid type=\"imdb\">tt{:07}</uniqueid>\n</movie>\n",
                100_000 + i,
                1_000_000 + i,
            ),
        )
        .expect("nfo");
        write_png(&dir.join("poster.png"), 20, 30, i);
        write_png(&dir.join("fanart.png"), 32, 18, i);
    }
    let tv_root = root.join("tv");
    let per_season = episodes.div_ceil(3).max(1);
    for s in 0..series {
        let year = 1990 + s % 30;
        let dir = tv_root.join(format!("Series Name {s:03} ({year})"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("tvshow.nfo"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tvshow>\n  \
                 <title>Series Name {s:03}</title>\n  <plot>{PLOT}</plot>\n  \
                 <year>{year}</year>\n  <genre>Drama</genre>\n  \
                 <uniqueid type=\"tvdb\" default=\"true\">{}</uniqueid>\n  \
                 <uniqueid type=\"tmdb\">{}</uniqueid>\n  \
                 <uniqueid type=\"imdb\">tt{:07}</uniqueid>\n</tvshow>\n",
                300_000 + s,
                200_000 + s,
                2_000_000 + s,
            ),
        )
        .expect("nfo");
        write_png(&dir.join("poster.png"), 20, 30, s);
        write_png(&dir.join("fanart.png"), 32, 18, s);
        for e in 0..episodes {
            let season = e / per_season + 1;
            let number = e % per_season + 1;
            let season_dir = dir.join(format!("Season {season:02}"));
            std::fs::create_dir_all(&season_dir).expect("mkdir");
            let stem = format!("Series Name {s:03} S{season:02}E{number:02}");
            std::fs::write(season_dir.join(format!("{stem}.mkv")), b"").expect("mkv");
            std::fs::write(
                season_dir.join(format!("{stem}.nfo")),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<episodedetails>\n  \
                     <title>Episode title {number}</title>\n  <plot>{PLOT}</plot>\n  \
                     <season>{season}</season>\n  <episode>{number}</episode>\n  \
                     <uniqueid type=\"tvdb\" default=\"true\">{}</uniqueid>\n\
                     </episodedetails>\n",
                    5_000_000 + s * 1000 + e,
                ),
            )
            .expect("nfo");
            write_png(&season_dir.join(format!("{stem}-thumb.png")), 32, 18, e);
        }
    }
}

/// MiB, for the report.
#[allow(clippy::cast_precision_loss)]
fn mib(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// A scanner over a fresh file database with a movie library on
/// `root/movies` and a TV library on `root/tv`, probing images for real.
async fn corpus_scanner(
    root: &Path,
) -> (
    Database,
    Arc<FerrofinItemPersistenceService>,
    LibraryScanner,
) {
    let db = Database::connect(&format!("sqlite://{}", root.join("ferrofin.db").display()))
        .await
        .expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    for (name, kind, dir) in [
        ("Movies", CollectionTypeOptions::movies, "movies"),
        ("Shows", CollectionTypeOptions::tvshows, "tv"),
    ] {
        vf.add_virtual_folder(
            name,
            Some(kind),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: root.join(dir).to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
    }
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
        ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), root.join("cache")),
    );
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
        .with_items(items)
        .with_image_processor(processor)
        .with_progress_every(0);
    (db, persistence, scanner)
}

/// The heap of the scan's two preread maps when built for every stored row
/// at once, in the shapes the scan keeps them in: what reading them for the
/// whole plan up front holds.
struct WholePlanMaps {
    rows: usize,
    with_provider_ids: usize,
    complete_images: usize,
    provider_kept: usize,
    provider_peak: usize,
    image_kept: usize,
    image_peak: usize,
}

/// Builds the two maps of [`WholePlanMaps`] from every stored row and
/// measures them.
async fn whole_plan_maps(
    db: &Database,
    persistence: &FerrofinItemPersistenceService,
) -> WholePlanMaps {
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar::<_, String>(r#"SELECT "Id" FROM "BaseItems""#)
        .fetch_all(db.pool())
        .await
        .expect("ids")
        .iter()
        .filter_map(|id| uuid::Uuid::parse_str(id).ok())
        .collect();
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    let provider_ids: HashMap<String, Vec<(String, String)>> = persistence
        .provider_ids_for_items(&ids)
        .await
        .expect("provider ids")
        .into_iter()
        .map(|(id, ids)| (guid_to_db(id), ids))
        .collect();
    let provider_peak = PEAK.load(Ordering::Relaxed).saturating_sub(live);
    let provider_kept = LIVE.load(Ordering::Relaxed).saturating_sub(live);
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    #[allow(clippy::type_complexity)]
    let images: HashMap<String, (i32, i32, Option<String>, chrono::DateTime<chrono::Utc>)> =
        persistence
            .image_metadata_for_items(&ids)
            .await
            .expect("image metadata")
            .into_iter()
            .filter(|row| row.width > 0 && row.height > 0 && row.blur_hash.is_some())
            .map(|row| {
                (
                    row.path,
                    (row.width, row.height, row.blur_hash, row.date_modified),
                )
            })
            .collect();
    let image_peak = PEAK.load(Ordering::Relaxed).saturating_sub(live);
    let image_kept = LIVE.load(Ordering::Relaxed).saturating_sub(live);
    WholePlanMaps {
        rows: ids.len(),
        with_provider_ids: provider_ids.len(),
        complete_images: images.len(),
        provider_kept,
        provider_peak,
        image_kept,
        image_peak,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rescan_heap_by_phase() {
    if std::env::var("FERROFIN_SCAN_MEMORY_BENCH").is_err() {
        eprintln!("library_scan_memory_bench: set FERROFIN_SCAN_MEMORY_BENCH=1 to run");
        return;
    }
    let movies = env_usize("FERROFIN_SCAN_MEMORY_MOVIES", 3000);
    let series = env_usize("FERROFIN_SCAN_MEMORY_SERIES", 250);
    let episodes = env_usize("FERROFIN_SCAN_MEMORY_EPISODES", 30);
    let tmp = tempfile::tempdir().expect("tmp");
    write_corpus(tmp.path(), movies, series, episodes);
    let (db, persistence, scanner) = corpus_scanner(tmp.path()).await;

    let first = scanner.scan_all().await.expect("first scan");
    eprintln!("first scan: {first:?}");
    // Settles anything the first scan left to a second look.
    let second = scanner.scan_all().await.expect("second scan");
    eprintln!("second scan: {second:?}");

    // Process-wide: the scan's events can come from any runtime worker. This
    // binary holds this one test, so nothing else sees the subscriber.
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(
        PhaseLayer.with_filter(Targets::new().with_target("ferrofin_core", tracing::Level::DEBUG)),
    ))
    .expect("global subscriber");
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let rescan = scanner.scan_all().await.expect("re-scan");
    let scan_peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    let settled = LIVE.load(Ordering::Relaxed).saturating_sub(base);
    eprintln!("re-scan: {rescan:?}");
    let since = |mark: &AtomicUsize| mark.load(Ordering::Relaxed).saturating_sub(base);
    let maps = whole_plan_maps(&db, &persistence).await;

    let planned_items = PLANNED_ITEMS.load(Ordering::Relaxed);
    let per_item = |bytes: usize| bytes / planned_items.max(1);
    eprintln!(
        "corpus: {movies} movies, {series} series x {episodes} episodes; {planned_items} planned \
         items, {} rows read back, {} with provider ids, {} complete images",
        maps.rows, maps.with_provider_ids, maps.complete_images
    );
    for (label, bytes) in [
        ("re-scan peak heap            ", scan_peak),
        ("live at 'library scan planned'", since(&PLANNED_LIVE)),
        ("live at the first item       ", since(&FIRST_ITEM_LIVE)),
        ("most live at any item        ", since(&MAX_ITEM_LIVE)),
    ] {
        eprintln!(
            "{label}: {:7.2} MiB ({} B/item)",
            mib(bytes),
            per_item(bytes)
        );
    }
    eprintln!(
        "planning peak                : {:7.2} MiB (the plan, its listings and the planning sets \
         are live at 'library scan planned'; the prereads follow before the first item)",
        mib(since(&PLANNED_PEAK))
    );
    eprintln!("settled after the re-scan    : {:7.2} MiB", mib(settled));
    for (label, kept, peak) in [
        (
            "provider-id map, whole plan  ",
            maps.provider_kept,
            maps.provider_peak,
        ),
        (
            "image-metadata map, whole plan",
            maps.image_kept,
            maps.image_peak,
        ),
    ] {
        eprintln!(
            "{label}: {:7.2} MiB kept ({} B/item), {:.2} MiB peak while built",
            mib(kept),
            per_item(kept),
            mib(peak)
        );
    }
    assert!(planned_items > 0, "the planned line was seen");
    db.close().await;
}
