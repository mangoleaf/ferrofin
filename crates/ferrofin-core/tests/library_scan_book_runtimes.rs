//! A book's runtime is its page count × 10000 ticks, as upstream's
//! `ProbeProvider.FetchAsync(Book)` sets it (Jellyfin PR 17311): a comic
//! archive's non-directory entries, a PDF's pages. An EPUB keeps one second,
//! and so do the Kindle formats (an accepted divergence: upstream leaves them
//! none). The count runs on upstream's probe triggers (a new book, a requested
//! refresh, a changed file), and on any scan of a comic or PDF still holding the
//! one-second placeholder. A plain rescan of a counted, unchanged book does not
//! reopen it.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

const ONE_SECOND: i64 = 10_000_000;

/// Writes a `.cbz` holding a `pages/` directory and `files` page entries.
fn write_cbz(path: &Path, files: usize) {
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).expect("create cbz"));
    let options = zip::write::SimpleFileOptions::default();
    writer.add_directory("pages/", options).expect("directory");
    for page in 0..files {
        writer
            .start_file(format!("pages/{page:03}.jpg"), options)
            .expect("entry");
        writer.write_all(b"page").expect("write");
    }
    writer.finish().expect("finish cbz");
}

/// Writes a PDF of `pages` pages with a classic cross-reference table.
fn write_pdf(path: &Path, pages: u64) {
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        ),
    ];
    objects.extend((0..pages).map(|_| "<< /Type /Page /Parent 2 0 R >>".to_owned()));
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f\r\n", objects.len() + 1).as_bytes(),
    );
    for at in offsets {
        pdf.extend_from_slice(format!("{at:010} 00000 n\r\n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, pdf).expect("write pdf");
}

/// Moves `path`'s mtime `secs` seconds on, past the change tolerance.
fn age(path: &Path, secs: u64) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    let mtime = file.metadata().expect("stat").modified().expect("mtime");
    file.set_modified(mtime + std::time::Duration::from_secs(secs))
        .expect("set mtime");
}

/// The stored `RunTimeTicks` of the book whose path ends in `suffix`.
async fn runtime(db: &Database, suffix: &str) -> Option<i64> {
    sqlx::query_scalar(r#"SELECT "RunTimeTicks" FROM "BaseItems" WHERE "Path" LIKE '%' || ?1"#)
        .bind(suffix)
        .fetch_one(db.pool())
        .await
        .expect("book row")
}

/// The stored `DateLastSaved` of the book whose path ends in `suffix`.
async fn saved(db: &Database, suffix: &str) -> Option<String> {
    sqlx::query_scalar(r#"SELECT "DateLastSaved" FROM "BaseItems" WHERE "Path" LIKE '%' || ?1"#)
        .bind(suffix)
        .fetch_one(db.pool())
        .await
        .expect("book row")
}

/// Sets the stored runtime of the book whose path ends in `suffix`, as a
/// release before the count left it.
async fn set_runtime(db: &Database, suffix: &str, ticks: i64) {
    sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = ?2 WHERE "Path" LIKE '%' || ?1"#)
        .bind(suffix)
        .bind(ticks)
        .execute(db.pool())
        .await
        .expect("set runtime");
}

/// A books library over `media`, and a scanner for it whose comic archives
/// may hold `max_entries` entries.
async fn books_library(
    tmp: &Path,
    media: &Path,
    max_entries: Option<u32>,
) -> (Database, LibraryScanner) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Books",
        Some(CollectionTypeOptions::books),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let mut scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items);
    if let Some(max) = max_entries {
        scanner = scanner.with_book_archive_max_entries(max);
    }
    (db, scanner)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_book_runtime_comes_from_its_pages_and_refreshes_with_its_probe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("books");
    std::fs::create_dir_all(&media).expect("media");
    write_cbz(&media.join("Saga v01.cbz"), 3);
    write_pdf(&media.join("Paper.pdf"), 12);
    for name in ["Novel.epub", "Kindle.mobi"] {
        std::fs::write(media.join(name), b"").expect("book");
    }
    std::fs::write(media.join("Broken.cbz"), b"not an archive").expect("book");
    let (db, scanner) = books_library(tmp.path(), &media, None).await;
    scanner.scan_all().await.expect("scan");

    // Three pages; the directory entry does not count.
    assert_eq!(runtime(&db, "/Saga v01.cbz").await, Some(30_000));
    assert_eq!(runtime(&db, "/Paper.pdf").await, Some(120_000));
    assert_eq!(runtime(&db, "/Novel.epub").await, Some(ONE_SECOND));
    assert_eq!(runtime(&db, "/Kindle.mobi").await, Some(ONE_SECOND));
    // Unreadable: the new row keeps the one-second placeholder.
    assert_eq!(runtime(&db, "/Broken.cbz").await, Some(ONE_SECOND));

    // A row an older release stored with one second takes its page count on
    // the next plain scan (owner decision on gl-495: runtimes refresh on
    // rescan), while a book already counted is not reopened.
    set_runtime(&db, "/Saga v01.cbz", ONE_SECOND).await;
    set_runtime(&db, "/Paper.pdf", 12_345).await;
    scanner.scan_all().await.expect("rescan");
    assert_eq!(runtime(&db, "/Saga v01.cbz").await, Some(30_000));
    assert_eq!(runtime(&db, "/Paper.pdf").await, Some(12_345));
    // A rescan that changes no runtime saves nothing: neither a book still at
    // one second because it cannot be counted, nor a counted one.
    let last_saved = (
        saved(&db, "/Broken.cbz").await,
        saved(&db, "/Saga v01.cbz").await,
    );
    scanner.scan_all().await.expect("rescan without changes");
    assert_eq!(
        (
            saved(&db, "/Broken.cbz").await,
            saved(&db, "/Saga v01.cbz").await,
        ),
        last_saved
    );

    // A changed file is probed again on the next scan.
    write_cbz(&media.join("Saga v01.cbz"), 5);
    age(&media.join("Saga v01.cbz"), 10);
    scanner.scan_all().await.expect("rescan a changed file");
    assert_eq!(runtime(&db, "/Saga v01.cbz").await, Some(50_000));
    assert_eq!(runtime(&db, "/Paper.pdf").await, Some(12_345));

    // "Search for missing metadata" runs every provider, the probe too.
    scanner
        .scan_with(
            None,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                image_refresh_mode: MetadataRefreshMode::None,
                ..MetadataRefreshOptions::default()
            },
        )
        .await
        .expect("refresh");
    assert_eq!(runtime(&db, "/Paper.pdf").await, Some(120_000));
    assert_eq!(runtime(&db, "/Saga v01.cbz").await, Some(50_000));
    // Unreadable files keep their runtime through all of it.
    assert_eq!(runtime(&db, "/Broken.cbz").await, Some(ONE_SECOND));
}

/// An archive past `book_archive_max_entries` is not counted: a new book keeps
/// the one-second placeholder, a stored one keeps its runtime.
#[tokio::test(flavor = "multi_thread")]
async fn an_archive_past_the_entry_cap_is_not_counted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("books");
    std::fs::create_dir_all(&media).expect("media");
    write_cbz(&media.join("Small.cbz"), 2);
    write_cbz(&media.join("Large.cbz"), 6);
    let (db, scanner) = books_library(tmp.path(), &media, Some(5)).await;
    scanner.scan_all().await.expect("scan");
    assert_eq!(runtime(&db, "/Small.cbz").await, Some(20_000));
    assert_eq!(runtime(&db, "/Large.cbz").await, Some(ONE_SECOND));
}
