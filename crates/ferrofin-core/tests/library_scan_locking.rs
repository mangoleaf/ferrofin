//! What a library scan may and may not touch on a **metadata-locked** item.
//!
//! The walk's `locked` flag is the item's stored row's `IsLocked`; the
//! locked-item set the scan reads once (`ItemRepository::locked_item_ids`)
//! serves only the items whose row it did not read. These tests pin that the
//! flag still means `IsLocked`.
//!
//! The NFO is the assertion with teeth. The user-owned *metadata* columns are
//! protected a second time inside the scan upsert (`CASE WHEN "IsLocked" = 1
//! THEN "<col>" ELSE excluded."<col>" END`), so a test asserting on those would
//! still pass with the loop's flag stuck at `false`. The external ids an NFO
//! pins have no such SQL backstop — they reach `BaseItemProviders` only when
//! the reader runs, which upstream never does for a locked item
//! (`RefreshWithProviders` returns on `IsLocked` before its local providers).
//!
//! Artwork is the other way round: upstream validates a locked item's local
//! images like any other's (`CanRefreshImages` enables `ILocalImageProvider`
//! before its `IsLocked` check), so the scan rediscovers them.
//!
//! A lock, an unlock or an edit made while the scan runs
//! (`PriorityLane::edited`) is seen by the walk before it reaches the item:
//! upstream's metadata editor changes the cached item the refresh merges
//! onto. One made while the walk refreshes the item leaves it as the edit
//! wrote it (`PriorityLane::edited_since`).

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::library_scan::{
    LaneRefresh, PriorityLane, ScanCancel, ScanOutcome, ScanPasses, ScanRun,
};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_providers::TmdbClient;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::providers::{
    DynamicMetadataLookup, DynamicMetadataProvider, DynamicMetadataResult, ItemUpdateType,
    MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use uuid::Uuid;

/// Builds a one-movie library (media file + a poster beside it) and wires a
/// scanner over an in-memory database.
async fn one_movie_library(root: &Path) -> (LibraryScanner, Database) {
    let media = root.join("movies");
    let folder = media.join("Movie 0001 (2020)");
    std::fs::create_dir_all(&folder).expect("fixture dirs");
    std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").expect("media file");
    std::fs::write(folder.join("poster.jpg"), b"jpeg").expect("poster");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");

    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Movies",
        Some(CollectionTypeOptions::movies),
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
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items);
    (scanner, db)
}

/// How many artwork rows the library currently holds.
async fn image_rows(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItemImageInfos""#)
        .fetch_one(db.pool())
        .await
        .expect("count images")
}

/// Sets `IsLocked` on the movie row, as the metadata editor's lock does.
async fn set_locked(db: &Database, locked: i64) {
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = ?1 WHERE "Type" LIKE '%Movies.Movie'"#)
        .bind(locked)
        .execute(db.writer())
        .await
        .expect("set lock");
}

#[tokio::test]
async fn a_locked_item_still_validates_its_local_artwork() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;

    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        image_rows(&db).await,
        1,
        "the first scan discovers the poster"
    );

    // The item is locked and its image rows are lost; the poster is still
    // on disk beside the movie.
    set_locked(&db, 1).await;
    sqlx::query(r#"DELETE FROM "BaseItemImageInfos""#)
        .execute(db.writer())
        .await
        .expect("clear images");

    scanner.scan_all().await.expect("rescan while locked");
    assert_eq!(
        image_rows(&db).await,
        1,
        "local image validation runs whatever the lock: the poster is rediscovered"
    );
}

/// The `Tmdb` id rows the movie carries.
async fn tmdb_ids(db: &Database) -> Vec<String> {
    sqlx::query_scalar(
        r#"SELECT "ProviderValue" FROM "BaseItemProviders" WHERE "ProviderId" = 'Tmdb'"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("provider ids")
}

/// Moves `path`'s mtime an hour ahead, past the NFO reader's one-minute
/// tolerance over `DateLastSaved`.
fn touch_ahead(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3_600))
        .expect("set mtime");
}

#[tokio::test]
async fn a_locked_item_never_reads_its_nfo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    scanner.scan_all().await.expect("first scan");

    set_locked(&db, 1).await;
    let nfo = tmp
        .path()
        .join("movies")
        .join("Movie 0001 (2020)")
        .join("Movie 0001 (2020).nfo");
    std::fs::write(&nfo, "<movie><tmdbid>603</tmdbid></movie>").expect("nfo");
    touch_ahead(&nfo);
    scanner.scan_all().await.expect("rescan while locked");
    assert!(
        tmdb_ids(&db).await.is_empty(),
        "the NFO's pinned id never reaches a locked item"
    );

    // Control: unlocked, the same (changed) NFO is read.
    set_locked(&db, 0).await;
    scanner.scan_all().await.expect("rescan while unlocked");
    assert_eq!(tmdb_ids(&db).await, ["603"]);
}

#[tokio::test]
async fn the_scan_reads_the_locked_set_from_the_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    scanner.scan_all().await.expect("first scan");

    let items = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));
    assert!(
        items
            .locked_item_ids()
            .await
            .expect("locked ids")
            .is_empty(),
        "a freshly scanned library locks nothing"
    );

    set_locked(&db, 1).await;
    assert_eq!(
        items.locked_item_ids().await.expect("locked ids").len(),
        1,
        "the locked movie is the one row the scan's per-scan read returns"
    );
}

/// Adds a second movie to [`one_movie_library`]'s library.
fn add_second_movie(root: &Path) {
    let folder = root.join("movies").join("Movie 0002 (2020)");
    std::fs::create_dir_all(&folder).expect("fixture dirs");
    std::fs::write(folder.join("Movie 0002 (2020).mkv"), b"").expect("media file");
}

/// A priority lane with no refresh to serve. At its first call once the
/// walk is past its first item, and so past the read of the stored-row window
/// holding the movies, it runs `edit` on the movie rows as the metadata
/// editor would, and reports the movies it edited as edited outside the scan.
struct EditingLane {
    db: Database,
    /// The edit: an `UPDATE` of the movie rows `RETURNING "Id"`.
    edit: &'static str,
    /// The walk's last reported percent: below 96 while it walks.
    percent: Arc<Mutex<Option<f64>>>,
    /// The edited movies, once the edit landed.
    edited: Mutex<Option<Vec<Uuid>>>,
    /// The edited movies the scan has not taken yet.
    unread: Mutex<Vec<Uuid>>,
}

impl PriorityLane for EditingLane {
    fn next(&self) -> Option<LaneRefresh> {
        // The walk reports `scanned * 96 / total` before each item, and
        // nothing reports a percent in (0, 96) before it: the first one is
        // at its second item, after its first window read. Were the
        // planning to report one, the edit would land before that read and
        // the tests would pass without the re-read.
        let walking = self
            .percent
            .lock()
            .expect("percent")
            .is_some_and(|p| p > 0.0 && p < 96.0);
        let mut edited = self.edited.lock().expect("edited");
        if walking && edited.is_none() {
            let ids: Vec<String> = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(sqlx::query_scalar(self.edit).fetch_all(self.db.writer()))
            })
            .expect("edit");
            let ids: Vec<Uuid> = ids.iter().map(|id| id.parse().expect("id")).collect();
            self.unread.lock().expect("unread").extend(&ids);
            *edited = Some(ids);
        }
        None
    }

    fn done(&self, _refresh: LaneRefresh, _outcome: &Result<ScanOutcome, ServiceError>) {}

    fn edited(&self) -> Vec<Uuid> {
        std::mem::take(&mut *self.unread.lock().expect("unread"))
    }
}

/// Rescans every library with `options` while `edit` of the movies lands
/// mid-walk, after their stored rows were read.
async fn rescan_with_edit_mid_walk(
    scanner: &LibraryScanner,
    db: &Database,
    options: &MetadataRefreshOptions,
    edit: &'static str,
) {
    let percent = Arc::new(Mutex::new(None));
    let report = {
        let percent = Arc::clone(&percent);
        move |p: f64| *percent.lock().expect("percent") = Some(p)
    };
    let lane = EditingLane {
        db: db.clone(),
        edit,
        percent,
        edited: Mutex::new(None),
        unread: Mutex::new(Vec::new()),
    };
    let cancel = ScanCancel::new();
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(options, options, &cancel)
                .with_progress(&report)
                .with_lane(&lane),
        )
        .await
        .expect("rescan");
    let edited = lane.edited.lock().expect("edited").clone();
    assert_eq!(
        edited.map(|ids| ids.len()),
        Some(2),
        "the edit landed while the walk was between items"
    );
}

/// An edit made over the API while a saving scan runs, to an item the scan
/// has read but not reached yet, survives the scan's save of that item: the
/// walk reads the item again before it merges onto it.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_mid_scan_survives_the_scan_saving_the_item() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    add_second_movie(tmp.path());
    scanner.scan_all().await.expect("first scan");

    // A full refresh saves every item.
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    rescan_with_edit_mid_walk(
        &scanner,
        &db,
        &options,
        r#"UPDATE "BaseItems" SET "Overview" = 'Typed mid-scan'
           WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
    )
    .await;
    let overviews: Vec<Option<String>> = sqlx::query_scalar(
        r#"SELECT "Overview" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie' ORDER BY "Name""#,
    )
    .fetch_all(db.pool())
    .await
    .expect("overviews");
    assert_eq!(
        overviews,
        [
            Some("Typed mid-scan".to_owned()),
            Some("Typed mid-scan".to_owned())
        ]
    );
}

/// A lock applied over the API while a scan runs is honoured for the item
/// the scan has not reached yet: its changed NFO is not read. The walk takes
/// the movies in listing order, so which one it reached before the lock
/// varies; only that one reads its NFO.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_applied_mid_scan_keeps_the_nfo_from_the_item_not_reached_yet() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    add_second_movie(tmp.path());
    scanner.scan_all().await.expect("first scan");

    for (movie, id) in [("Movie 0001 (2020)", 601), ("Movie 0002 (2020)", 602)] {
        let nfo = tmp
            .path()
            .join("movies")
            .join(movie)
            .join(format!("{movie}.nfo"));
        std::fs::write(&nfo, format!("<movie><tmdbid>{id}</tmdbid></movie>")).expect("nfo");
        touch_ahead(&nfo);
    }
    rescan_with_edit_mid_walk(
        &scanner,
        &db,
        &MetadataRefreshOptions::default(),
        r#"UPDATE "BaseItems" SET "IsLocked" = 1
           WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
    )
    .await;
    assert_eq!(
        tmdb_ids(&db).await.len(),
        1,
        "only the movie reached before the lock read its NFO"
    );

    // Control: unlocked, the same (changed) NFOs are read.
    set_locked(&db, 0).await;
    scanner.scan_all().await.expect("rescan while unlocked");
    let mut ids = tmdb_ids(&db).await;
    ids.sort();
    assert_eq!(ids, ["601", "602"]);
}

/// An unlock applied over the API while a scan runs is honoured for the item
/// the scan has not reached yet: it reads its changed NFO in this scan, not
/// the next one. The one reached before the unlock stays locked.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlock_applied_mid_scan_lets_the_item_not_reached_yet_read_its_nfo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    add_second_movie(tmp.path());
    scanner.scan_all().await.expect("first scan");
    set_locked(&db, 1).await;

    for (movie, id) in [("Movie 0001 (2020)", 601), ("Movie 0002 (2020)", 602)] {
        let nfo = tmp
            .path()
            .join("movies")
            .join(movie)
            .join(format!("{movie}.nfo"));
        std::fs::write(&nfo, format!("<movie><tmdbid>{id}</tmdbid></movie>")).expect("nfo");
        touch_ahead(&nfo);
    }
    rescan_with_edit_mid_walk(
        &scanner,
        &db,
        &MetadataRefreshOptions::default(),
        r#"UPDATE "BaseItems" SET "IsLocked" = 0
           WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
    )
    .await;
    assert_eq!(
        tmdb_ids(&db).await.len(),
        1,
        "only the movie reached after the unlock read its NFO"
    );

    // Control: the movie reached first skipped its NFO only for its lock.
    scanner.scan_all().await.expect("rescan while unlocked");
    let mut ids = tmdb_ids(&db).await;
    ids.sort();
    assert_eq!(ids, ["601", "602"]);
}

/// A priority lane that records an edit made over the API at the TheMovieDb
/// stand-in's first request once armed: inside the walk's refresh of the
/// movie, after the walk read its row. It serves `serve`, if set, as the
/// running scan's one lane refresh.
struct EditDuringFetch {
    db: Database,
    runtime: tokio::runtime::Handle,
    /// Edits the movie at the stand-in's next request.
    armed: AtomicBool,
    /// The edited ids, in order; a mark is how many there were.
    edits: Mutex<Vec<Uuid>>,
    serve: Mutex<Option<LaneRefresh>>,
}

impl EditDuringFetch {
    /// Overwrites the movie's overview as the metadata editor's save does,
    /// and records the edit.
    async fn edit(&self) {
        let ids: Vec<String> = sqlx::query_scalar(
            r#"UPDATE "BaseItems" SET "Overview" = 'Typed mid-pass'
               WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
        )
        .fetch_all(self.db.writer())
        .await
        .expect("edit");
        let mut edits = self.edits.lock().expect("edits");
        edits.extend(ids.iter().map(|id| id.parse::<Uuid>().expect("id")));
    }
}

impl PriorityLane for EditDuringFetch {
    fn next(&self) -> Option<LaneRefresh> {
        self.serve.lock().expect("serve").take()
    }

    fn done(&self, _refresh: LaneRefresh, outcome: &Result<ScanOutcome, ServiceError>) {
        outcome.as_ref().expect("the served refresh");
    }

    fn edit_mark(&self) -> u64 {
        self.edits.lock().expect("edits").len() as u64
    }

    fn edited_since(&self, id: Uuid, mark: u64) -> bool {
        let edits = self.edits.lock().expect("edits");
        let mark = usize::try_from(mark).expect("mark");
        edits.get(mark..).is_some_and(|since| since.contains(&id))
    }
}

/// A TheMovieDb stand-in on a local port that knows the movie as id 20, with
/// an overview, and runs `lane`'s edit before it answers once armed.
fn tmdb_stand_in(lane: Arc<EditDuringFetch>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            if lane.armed.swap(false, Ordering::SeqCst) {
                lane.runtime.block_on(lane.edit());
            }
            let (status, payload) = if request.contains("/search/movie") {
                (
                    "200 OK",
                    r#"{"results": [{"id": 20, "title": "Movie 0001", "release_date": "2020-01-01"}]}"#,
                )
            } else if request.contains("/movie/20?") {
                (
                    "200 OK",
                    r#"{"id": 20, "title": "Movie 0001", "overview": "From TMDB.", "release_date": "2020-01-01"}"#,
                )
            } else {
                ("404 Not Found", "{}")
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    format!("http://{addr}")
}

/// The movie's overview and refresh stamp.
async fn overview_and_stamp(db: &Database) -> (Option<String>, Option<String>) {
    sqlx::query_as(
        r#"SELECT "Overview", "DateLastRefreshed" FROM "BaseItems"
           WHERE "Type" LIKE '%Movies.Movie'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("movie")
}

/// [`one_movie_library`] with TheMovieDb on a stand-in that edits the movie
/// at its first request once armed, scanned once (unarmed).
async fn movie_with_tmdb(root: &Path) -> (LibraryScanner, Database, Arc<EditDuringFetch>) {
    let (scanner, db) = one_movie_library(root).await;
    let lane = Arc::new(EditDuringFetch {
        db: db.clone(),
        runtime: tokio::runtime::Handle::current(),
        armed: AtomicBool::new(false),
        edits: Mutex::new(Vec::new()),
        serve: Mutex::new(None),
    });
    let scanner = scanner.with_metadata(
        Arc::new(TmdbClient::new().with_base_url(&tmdb_stand_in(Arc::clone(&lane)))),
        root.join("metadata"),
    );
    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        overview_and_stamp(&db).await.0.as_deref(),
        Some("From TMDB."),
        "the first scan takes TheMovieDb's overview"
    );
    (scanner, db, lane)
}

/// A full refresh: the walk asks TheMovieDb again and saves the movie.
fn full_refresh() -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    }
}

/// Rescans every library with a full refresh, serving `lane`.
async fn full_rescan(scanner: &LibraryScanner, lane: &EditDuringFetch) {
    let options = full_refresh();
    let cancel = ScanCancel::new();
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_lane(lane),
        )
        .await
        .expect("rescan");
}

/// An edit made over the API while the walk refreshes that same item (here
/// while it waits on TheMovieDb) is not saved over: the walk leaves the item
/// as the edit wrote it, its refresh stamp too.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_while_the_walk_refreshes_the_item_is_not_saved_over() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, lane) = movie_with_tmdb(tmp.path()).await;
    let (_, first) = overview_and_stamp(&db).await;

    // Control: with no edit, the same rescan saves the movie and stamps it.
    full_rescan(&scanner, &lane).await;
    let (_, stamp) = overview_and_stamp(&db).await;
    assert_ne!(stamp, first, "an unedited movie is saved");

    lane.armed.store(true, Ordering::SeqCst);
    full_rescan(&scanner, &lane).await;

    assert_eq!(lane.edit_mark(), 1, "the edit landed inside the refresh");
    assert_eq!(
        overview_and_stamp(&db).await,
        (Some("Typed mid-pass".to_owned()), stamp)
    );
}

/// The same for an item refresh the running scan serves between its items
/// (a refresh asked for over the API): the walk it runs checks the running
/// scan's record of edits too.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_while_a_served_refresh_refreshes_the_item_is_not_saved_over() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, lane) = movie_with_tmdb(tmp.path()).await;
    let (_, stamp) = overview_and_stamp(&db).await;

    let movie = tmp
        .path()
        .join("movies")
        .join("Movie 0001 (2020)")
        .join("Movie 0001 (2020).mkv");
    *lane.serve.lock().expect("serve") = Some(LaneRefresh {
        target: ScanTarget::Items(vec![movie.to_string_lossy().into_owned()]),
        options: full_refresh(),
        ancestors: MetadataRefreshOptions::default(),
        passes: ScanPasses::Touched,
        cancel: ScanCancel::new(),
        key: 1,
    });
    lane.armed.store(true, Ordering::SeqCst);
    // The running scan's own pass refreshes nothing that is not due.
    let options = MetadataRefreshOptions::default();
    let cancel = ScanCancel::new();
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_lane(&*lane),
        )
        .await
        .expect("rescan");

    assert!(lane.serve.lock().expect("serve").is_none(), "served");
    assert_eq!(lane.edit_mark(), 1, "the edit landed inside the refresh");
    assert_eq!(
        overview_and_stamp(&db).await,
        (Some("Typed mid-pass".to_owned()), stamp)
    );
}

/// A provider manager whose metadata lock for `movie` is `lock`, announcing
/// each time the scan asks for it; every other item's lock is its own.
struct LockingProviders {
    movie: Uuid,
    lock: Arc<tokio::sync::Mutex<()>>,
    asked: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ProviderManager for LockingProviders {
    async fn lock_item_metadata(&self, item_id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        if item_id != self.movie {
            return Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
        }
        self.asked.notify_one();
        Arc::clone(&self.lock).lock_owned().await
    }
    async fn queue_refresh(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        Ok(ItemUpdateType::None)
    }
    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &ferrofin_model::providers::RemoteImageQuery,
    ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_metadata_options(
        &self,
        _item_id: Uuid,
    ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
        Ok(ferrofin_model::configuration::MetadataOptions::default())
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(Vec::new())
    }
}

/// [`LockingProviders`] for the one movie in `db`.
async fn locking_providers(db: &Database) -> Arc<LockingProviders> {
    let movie: String =
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#)
            .fetch_one(db.pool())
            .await
            .expect("movie");
    Arc::new(LockingProviders {
        movie: movie.parse().expect("id"),
        lock: Arc::new(tokio::sync::Mutex::new(())),
        asked: tokio::sync::Notify::new(),
    })
}

/// The walk checks for an edit only once it holds the item's metadata lock,
/// which the metadata editor holds across its writes: an edit that holds the
/// lock when the walk reaches the save is waited for and kept, though it is
/// recorded only after the walk asked for the lock.
#[tokio::test(flavor = "multi_thread")]
async fn the_walk_waits_for_an_edit_holding_the_items_lock_and_keeps_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, lane) = movie_with_tmdb(tmp.path()).await;
    let (_, stamp) = overview_and_stamp(&db).await;
    let providers = locking_providers(&db).await;
    let scanner = scanner.with_metadata_savers(providers.clone());

    // The editor holds the movie's lock while it writes.
    let editing = Arc::clone(&providers.lock).lock_owned().await;
    let edit = async {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            providers.asked.notified(),
        )
        .await
        .expect("the walk asks for the movie's lock before its save");
        lane.edit().await;
        drop(editing);
    };
    tokio::join!(full_rescan(&scanner, &lane), edit);

    assert_eq!(
        overview_and_stamp(&db).await,
        (Some("Typed mid-pass".to_owned()), stamp)
    );
}

/// A scan cancelled while it waits for an item's metadata lock (held by an
/// edit, or by a provider refresh waiting on a slow provider) stops there
/// instead of waiting the holder out, and leaves the item unsaved.
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_cancelled_while_it_waits_for_an_items_lock_stops_without_saving_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, lane) = movie_with_tmdb(tmp.path()).await;
    let stored = overview_and_stamp(&db).await;
    let providers = locking_providers(&db).await;
    let scanner = scanner.with_metadata_savers(providers.clone());

    // Held for the whole test: only the cancel can end the walk's wait.
    let _holder = Arc::clone(&providers.lock).lock_owned().await;
    let options = full_refresh();
    let cancel = ScanCancel::new();
    let stop = async {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            providers.asked.notified(),
        )
        .await
        .expect("the walk asks for the movie's lock before its save");
        cancel.cancel();
    };
    let scan = Box::pin(scanner.scan_target(
        &ScanTarget::All,
        ScanRun::new(&options, &options, &cancel).with_lane(&*lane),
    ));
    let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(scan, stop)
    })
    .await
    .expect("the cancel ends the walk's wait for the lock");

    assert!(outcome.expect("scan").stopped, "the scan stopped");
    assert_eq!(
        overview_and_stamp(&db).await,
        stored,
        "the movie is unsaved"
    );
}

/// A plugin image source that cancels the running scan when asked for
/// artwork: a cancel that lands in an item's artwork pass.
struct CancelOnArt(ScanCancel);

#[async_trait::async_trait]
impl DynamicMetadataProvider for CancelOnArt {
    fn name(&self) -> &'static str {
        "cancel-on-art"
    }
    fn library_gated(&self) -> bool {
        true
    }
    async fn lookup(
        &self,
        _item: &DynamicMetadataLookup,
    ) -> Result<Option<DynamicMetadataResult>, ServiceError> {
        Ok(None)
    }
    async fn images(
        &self,
        _item: &DynamicMetadataLookup,
        _wanted: &[ImageType],
    ) -> Result<Vec<(ImageType, Vec<u8>)>, ServiceError> {
        self.0.cancel();
        Ok(Vec::new())
    }
}

/// The movie's `DateLastSaved`, which every save of its row moves.
async fn date_last_saved(db: &Database) -> Option<String> {
    sqlx::query_scalar(
        r#"SELECT "DateLastSaved" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("movie")
}

/// A cancel that lands in a stored item's artwork pass still saves the item
/// with what the pass has (`ScanCancel`'s promise), its refresh stamp
/// cleared so the next scan repeats it: the lock is free, so the cancel
/// gives up no wait.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_in_the_artwork_pass_still_saves_an_item_whose_lock_is_free() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, lane) = movie_with_tmdb(tmp.path()).await;
    let saved = date_last_saved(&db).await;
    let providers = locking_providers(&db).await;
    let cancel = ScanCancel::new();
    let scanner = scanner
        .with_metadata_savers(providers)
        .with_dynamic_providers(vec![Arc::new(CancelOnArt(cancel.clone()))]);

    let options = MetadataRefreshOptions {
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..full_refresh()
    };
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_lane(&*lane),
        )
        .await
        .expect("rescan");

    assert!(cancel.is_cancelled(), "the artwork pass asked the plugin");
    assert_ne!(date_last_saved(&db).await, saved, "the movie was saved");
    assert_eq!(
        overview_and_stamp(&db).await.1,
        None,
        "left for the next scan"
    );
}
