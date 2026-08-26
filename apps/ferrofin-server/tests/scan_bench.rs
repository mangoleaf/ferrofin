//! Library-scan measurement harness (opt-in, `FERROFIN_SCAN_BENCH=1`).
//!
//! CI never runs this: without the env var the test returns immediately. It
//! exists because the scan is the longest-running user-visible operation in the
//! server and had no timing signal at all — "how many items per second do we
//! scan, and where does the time go" was unanswerable.
//!
//! Wiring mirrors the composition root's scanner (`state.rs`) for the pieces
//! that run offline: the real ffprobe seam (`FERROFIN_SCAN_BENCH_PROBE=1`), the
//! real image processor, people/chapters/stream repositories, and a file-backed
//! WAL database. The keyed remote providers stay off — they are feature-gated
//! off by default in production too.
//!
//! ```text
//! FERROFIN_SCAN_BENCH=1 FERROFIN_SCAN_BENCH_MOVIES=1000 \
//!   cargo test --release -p ferrofin-server --test scan_bench -- --nocapture
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ferrofin_core::item_persistence_service::FerrofinItemPersistenceService;
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;

/// Reads a `usize` knob from the environment, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Writes `bytes` to `path`, creating parents.
fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Builds a movie + TV fixture tree under `root` shaped like
/// `suite/perf/gen-fixtures.sh` (one directory per movie, an NFO sidecar, a
/// poster, and a media file), returning the movies and tv roots.
fn build_fixture(root: &Path, movies: usize, series: usize, eps: usize) -> (String, String) {
    let movie_root = root.join("movies");
    let tv_root = root.join("tv");
    // A tiny placeholder payload by default (ffprobe rejects it, exactly as it
    // does a corrupt file in the field); point FERROFIN_SCAN_BENCH_MEDIA at a
    // real clip to measure the probe against real streams.
    let sample = std::env::var("FERROFIN_SCAN_BENCH_MEDIA").ok().map_or_else(
        || vec![0u8; 1024],
        |p| std::fs::read(p).expect("sample media"),
    );
    let poster = std::env::var("FERROFIN_SCAN_BENCH_POSTER")
        .ok()
        .map_or_else(
            || vec![0u8; 512],
            |p| std::fs::read(p).expect("sample poster"),
        );
    let genres = ["Action", "Drama", "Comedy", "Thriller", "SciFi"];
    let actors = ["Alice Parity", "Bob Parity", "Carol Ferrofin"];
    for i in 1..=movies {
        let dir = movie_root.join(format!("Movie {i:04} (2020)"));
        write(&dir.join(format!("Movie {i:04} (2020).mkv")), &sample);
        write(&dir.join("poster.jpg"), &poster);
        let g1 = genres[i % genres.len()];
        let g2 = genres[(i + 2) % genres.len()];
        let a1 = actors[i % actors.len()];
        let a2 = actors[(i + 1) % actors.len()];
        let nfo = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<movie><title>Movie {i:04}</title>\
             <year>2020</year><genre>{g1}</genre><genre>{g2}</genre><studio>Parity Pictures</studio>\
             <actor><name>{a1}</name><role>Lead</role><type>Actor</type></actor>\
             <director>{a2}</director></movie>\n"
        );
        write(&dir.join("movie.nfo"), nfo.as_bytes());
    }
    for s in 1..=series {
        let base = tv_root.join(format!("Series {s:02}"));
        let g1 = genres[s % genres.len()];
        let nfo = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<tvshow><title>Series {s:02}</title>\
             <year>2021</year><genre>{g1}</genre><studio>Parity Pictures</studio></tvshow>\n"
        );
        write(&base.join("tvshow.nfo"), nfo.as_bytes());
        for e in 1..=eps {
            write(
                &base
                    .join("Season 01")
                    .join(format!("Series {s:02} S01E{e:02}.mkv")),
                &sample,
            );
        }
    }
    (
        movie_root.to_string_lossy().into_owned(),
        tv_root.to_string_lossy().into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_bench() {
    if std::env::var("FERROFIN_SCAN_BENCH").is_err() {
        eprintln!("scan_bench: set FERROFIN_SCAN_BENCH=1 to run");
        return;
    }
    let movies = env_usize("FERROFIN_SCAN_BENCH_MOVIES", 500);
    let series = env_usize("FERROFIN_SCAN_BENCH_SERIES", 50);
    let eps = env_usize("FERROFIN_SCAN_BENCH_EPISODES", 10);

    let tmp = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let (movie_root, tv_root) = build_fixture(tmp.path(), movies, series, eps);
    eprintln!("fixture built in {:?}", t0.elapsed());

    let db_path = tmp.path().join("ferrofin.db");
    let db = Database::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    db.run_migrations().await.unwrap();

    let persistence: Arc<dyn ferrofin_traits::persistence::ItemPersistenceService> =
        Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
    let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> = Arc::new(
        ferrofin_core::item_repository::FerrofinItemRepository::new(db.clone(), lookup),
    );
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        ferrofin_core::FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(Arc::clone(&persistence)),
    );
    for (name, kind, path) in [
        ("Movies", CollectionTypeOptions::movies, movie_root),
        ("Shows", CollectionTypeOptions::tvshows, tv_root),
    ] {
        vf.add_virtual_folder(
            name,
            Some(kind),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo { path }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
    }

    let mut scanner = ferrofin_core::LibraryScanner::new(
        Arc::clone(&vf),
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        Arc::clone(&persistence),
    )
    .with_localization(Arc::new(
        ferrofin_core::localization_manager::LocalizationManager::new("US"),
    ))
    .with_items(Arc::clone(&items))
    .with_people(Arc::new(
        ferrofin_core::people_repository::FerrofinPeopleRepository::new(db.clone()),
    ))
    .with_image_processor(Arc::new(ferrofin_drawing::ImageProcessor::new(
        Arc::new(ferrofin_drawing::ImageCrateEncoder::new()),
        tmp.path().join("imagecache").to_string_lossy().into_owned(),
    )))
    .with_metadata_dir(tmp.path().join("metadata"))
    .with_probe_concurrency(env_usize("FERROFIN_SCAN_PROBE_CONCURRENCY", 4))
    .with_progress_every(0);
    if std::env::var("FERROFIN_SCAN_BENCH_PROBE").is_ok() {
        scanner = scanner.with_probe(
            Arc::new(ferrofin_mediaencoding::MediaEncoderImpl::new(
                Arc::new(ferrofin_mediaencoding::TokioTranscoder::new()),
                "ffmpeg".to_owned(),
                "ffprobe".to_owned(),
                ferrofin_mediaencoding::MediaEncoderConfig {
                    analyze_duration: None,
                    probe_size: None,
                    threads: 0,
                    temp_dir: tmp.path().join("temp"),
                    ffmpeg_version: None,
                },
            )),
            Arc::new(
                ferrofin_core::media_stream_repository::FerrofinMediaStreamRepository::new(
                    db.clone(),
                ),
            ),
            Arc::new(ferrofin_core::chapter_repository::FerrofinChapterRepository::new(db.clone())),
        );
    }

    report("COLD  scan", &scanner).await;
    report("RESCAN    ", &scanner).await;
}

/// Runs one scan and prints its item count, wall time and items/second.
async fn report(label: &str, scanner: &ferrofin_core::LibraryScanner) {
    let t = Instant::now();
    let count = scanner.scan_all().await.unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let rate = count as f64 / elapsed;
    eprintln!("{label}: {count} items in {elapsed:.3}s => {rate:.1} items/s");
}
