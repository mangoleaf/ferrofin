//! "Date added behavior for new content" (`MetadataConfiguration.
//! UseFileCreationTimeForDateAdded`, Dashboard → Libraries → Display) end to
//! end through real scans (`PLAN_SCAN_CHANGE_DETECTION` 6D):
//!
//! - "Use date scanned into the library" dates a new episode by the moment
//!   Ferrofin first detects it — a library scan and a watcher/webhook scan
//!   alike — never by its file's times, and its series' `DateLastMediaAdded`
//!   follows (`ResolverHelper.SetDateCreated`);
//! - "Use file creation date" (the default) dates it by the file's creation
//!   time;
//! - a rescan never moves an unchanged item's date, in either mode, and
//!   switching the mode re-dates nothing;
//! - a file whose modification time drifted is re-dated to its creation time
//!   under "Use file creation date" and keeps its date under "Use date
//!   scanned" (`MetadataService.BeforeSaveInternal`);
//! - a date a reader supplies — an NFO `<dateadded>`, a photo's EXIF date —
//!   replaces a stored item's date whenever that reader runs, in either mode
//!   (`MetadataService.MergeData`).
//!
//! The file's mtime is set years back to distinguish Linux's source runtime
//! clock (the older of ctime and mtime) from statx birth time. Other platforms
//! retain their native creation-time backend. "Detected" is asserted against
//! a window opened after the file was written.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, MetadataConfiguration};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;
use uuid::Uuid;

/// A TV library whose scanner reads "Use file creation date" from
/// `file_creation`, as the server reads the dashboard's saved document.
struct Fixture {
    db: Database,
    items: Arc<dyn ItemRepository>,
    scanner: LibraryScanner,
    media: PathBuf,
    file_creation: Arc<AtomicBool>,
}

impl Fixture {
    async fn new(root: &Path, file_creation: bool) -> Self {
        let media = root.join("tv");
        std::fs::create_dir_all(media.join("Show").join("Season 01")).expect("mkdir");
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "TV",
            Some(CollectionTypeOptions::tvshows),
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
        let file_creation = Arc::new(AtomicBool::new(file_creation));
        let read = Arc::clone(&file_creation);
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_items(Arc::clone(&items))
            .with_progress_every(0)
            .with_metadata_configuration(move || MetadataConfiguration {
                use_file_creation_time_for_date_added: read.load(Ordering::SeqCst),
            });
        Self {
            db,
            items,
            scanner,
            media,
            file_creation,
        }
    }

    /// Switches the date-added rule, as a dashboard save does.
    fn use_file_creation(&self, on: bool) {
        self.file_creation.store(on, Ordering::SeqCst);
    }

    fn season(&self) -> PathBuf {
        self.media.join("Show").join("Season 01")
    }

    /// Writes episode `n` with an mtime years back, then waits a moment so
    /// the file's birth lies clearly before whatever happens next.
    fn add_episode(&self, n: u32) -> PathBuf {
        let path = self.season().join(format!("Show - S01E0{n}.mkv"));
        std::fs::write(&path, b"x").expect("write");
        set_mtime(&path, "2001-01-01T00:00:00Z");
        std::thread::sleep(std::time::Duration::from_millis(20));
        path
    }

    async fn row(&self, kind: BaseItemKind, path: &Path) -> BaseItemEntity {
        let id = derive_item_id(kind, &path.to_string_lossy()).expect("id");
        self.items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row")
    }

    async fn episode(&self, path: &Path) -> BaseItemEntity {
        self.row(BaseItemKind::Episode, path).await
    }

    async fn series_last_media_added(&self) -> Option<DateTime<Utc>> {
        self.row(BaseItemKind::Series, &self.media.join("Show"))
            .await
            .date_last_media_added
    }

    async fn scan(&self) {
        self.scanner.scan_all().await.expect("scan");
    }

    /// The disk watcher's (or a webhook's) report of `path`.
    async fn report(&self, path: &Path) {
        self.scanner
            .scan_paths(&[path.to_string_lossy().into_owned()])
            .await
            .expect("watcher scan");
    }

    /// Every item's `DateCreated`, by id.
    async fn dates(&self) -> Vec<(String, Option<String>)> {
        sqlx::query_as(
            r#"SELECT "Id", "DateCreated" FROM "BaseItems"
               WHERE "TopParentId" IS NOT NULL AND "TopParentId" <> '' ORDER BY "Id""#,
        )
        .fetch_all(self.db.pool())
        .await
        .expect("dates")
    }

    /// Sets an item's stored `DateCreated`, as an older scan, an adopted
    /// database or an NFO `<dateadded>` could have left it.
    async fn set_date_created(&self, kind: BaseItemKind, path: &Path, at: &str) {
        let id: Uuid = derive_item_id(kind, &path.to_string_lossy()).expect("id");
        sqlx::query(r#"UPDATE "BaseItems" SET "DateCreated" = ?1 WHERE "Id" = ?2"#)
            .bind(datetime_to_db(parse(at)))
            .bind(guid_to_db(id))
            .execute(self.db.writer())
            .await
            .expect("update");
    }
}

fn parse(at: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(at)
        .expect("rfc3339")
        .with_timezone(&Utc)
}

fn set_mtime(path: &Path, at: &str) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(parse(at).into())
        .expect("set mtime");
}

/// .NET's Linux backend uses the older of ctime and mtime. Other Unix
/// backends can expose birth time, falling back to the same oldest-time rule.
fn creation_time(path: &Path) -> DateTime<Utc> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).expect("stat");
    let birth = if cfg!(target_os = "linux") {
        None
    } else {
        meta.created().ok()
    };
    let ctime = DateTime::from_timestamp(
        meta.ctime(),
        u32::try_from(meta.ctime_nsec()).expect("nanos"),
    )
    .expect("ctime");
    let mtime = DateTime::<Utc>::from(meta.modified().expect("mtime"));
    birth.map_or_else(|| ctime.min(mtime), DateTime::<Utc>::from)
}

/// `date` as the table stores it (100 ns ticks), for comparing with a
/// stored date read back.
fn stored(date: DateTime<Utc>) -> String {
    datetime_to_db(date)
}

fn assert_detected_within(
    what: &str,
    date: Option<DateTime<Utc>>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) {
    let date = date.unwrap_or_else(|| panic!("{what} has no DateCreated"));
    assert!(
        (from..=to).contains(&date),
        "{what} is dated by its detection: {date} not in {from}..={to}"
    );
}

/// "Use date scanned into the library": a library scan and a watcher scan
/// both date the new episode by the moment they found it, not by its file,
/// and the series' newest-media date follows each at once.
#[tokio::test(flavor = "multi_thread")]
async fn date_scanned_dates_a_new_episode_by_its_detection_by_scan_or_watcher() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), false).await;
    let first = fx.add_episode(1);

    let before = Utc::now();
    fx.scan().await;
    let after = Utc::now();
    let one = fx.episode(&first).await;
    assert_detected_within("the scanned episode", one.date_created, before, after);
    assert!(
        one.date_created > Some(creation_time(&first)),
        "not the file's creation time"
    );
    assert_eq!(fx.series_last_media_added().await, one.date_created);

    let second = fx.add_episode(2);
    let before = Utc::now();
    fx.report(&second).await;
    let after = Utc::now();
    let two = fx.episode(&second).await;
    assert_detected_within("the watched episode", two.date_created, before, after);
    assert!(two.date_created > Some(creation_time(&second)));
    assert_eq!(
        fx.series_last_media_added().await,
        two.date_created,
        "the series' date added moves with the watcher event"
    );
    assert_eq!(
        fx.episode(&first).await.date_created,
        one.date_created,
        "the first episode keeps its date"
    );
}

/// "Use file creation date" (the default): a new episode is dated by its
/// file's creation time, by a library scan and a watcher scan alike.
#[tokio::test(flavor = "multi_thread")]
async fn file_creation_date_dates_a_new_episode_by_its_file() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), true).await;
    let first = fx.add_episode(1);
    fx.scan().await;
    let one = fx.episode(&first).await;
    assert_eq!(
        one.date_created.map(datetime_to_db),
        Some(stored(creation_time(&first)))
    );
    assert_eq!(fx.series_last_media_added().await, one.date_created);

    let second = fx.add_episode(2);
    fx.report(&second).await;
    let two = fx.episode(&second).await;
    assert_eq!(
        two.date_created.map(datetime_to_db),
        Some(stored(creation_time(&second)))
    );
    assert_eq!(fx.series_last_media_added().await, two.date_created);
}

/// Nothing re-dates an unchanged item: a library scan, a watcher scan, and
/// switching the rule either way between them.
#[rstest::rstest]
#[case::file_creation(true)]
#[case::date_scanned(false)]
#[tokio::test(flavor = "multi_thread")]
async fn a_rescan_never_moves_an_unchanged_items_date(#[case] file_creation: bool) {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), file_creation).await;
    let episode = fx.add_episode(1);
    fx.add_episode(2);
    fx.scan().await;
    let dates = fx.dates().await;
    assert!(dates.len() >= 4, "series, season, two episodes: {dates:?}");
    let last_media = fx.series_last_media_added().await;

    std::thread::sleep(std::time::Duration::from_millis(20));
    fx.scan().await;
    assert_eq!(fx.dates().await, dates, "a library rescan");
    fx.report(&episode).await;
    assert_eq!(fx.dates().await, dates, "a watcher scan of the episode");

    fx.use_file_creation(!file_creation);
    fx.scan().await;
    fx.report(&episode).await;
    assert_eq!(fx.dates().await, dates, "after switching the rule");
    assert_eq!(fx.series_last_media_added().await, last_media);
}

/// `BeforeSaveInternal`: a file whose modification time drifted is re-dated
/// to its creation time under "Use file creation date", by a library scan
/// or a watcher scan; under "Use date scanned" it keeps its date. An
/// unchanged file keeps its date either way.
#[rstest::rstest]
#[case::file_creation_by_scan(true, false)]
#[case::file_creation_by_watcher(true, true)]
#[case::date_scanned_by_scan(false, false)]
#[case::date_scanned_by_watcher(false, true)]
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_file_is_redated_only_under_file_creation_date(
    #[case] file_creation: bool,
    #[case] by_watcher: bool,
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), file_creation).await;
    let episode = fx.add_episode(1);
    fx.scan().await;
    let kept = "2020-01-01T00:00:00Z";
    fx.set_date_created(BaseItemKind::Episode, &episode, kept)
        .await;
    let rescan = async || {
        if by_watcher {
            fx.report(&episode).await;
        } else {
            fx.scan().await;
        }
    };

    rescan().await;
    assert_eq!(
        fx.episode(&episode).await.date_created,
        Some(parse(kept)),
        "an unchanged file keeps its date"
    );

    set_mtime(&episode, "2002-02-02T00:00:00Z");
    rescan().await;
    let row = fx.episode(&episode).await;
    assert_eq!(
        row.date_modified,
        Some(parse("2002-02-02T00:00:00Z")),
        "the drift was saved"
    );
    if file_creation {
        assert_eq!(
            row.date_created.map(datetime_to_db),
            Some(stored(creation_time(&episode))),
            "re-dated to the file's creation time"
        );
    } else {
        assert_eq!(
            row.date_created,
            Some(parse(kept)),
            "kept under date scanned"
        );
    }
    assert_eq!(
        fx.series_last_media_added().await,
        row.date_created,
        "the series follows"
    );
}

/// An edited NFO's `<dateadded>` lands on the stored episode it belongs to
/// (the NFO is newer than the item's last save, so its reader runs) —
/// through a library scan or a watcher report of the NFO — whichever the
/// date-added rule; the episode's file itself is unchanged. Upstream merges
/// a reader's `DateCreated` with the metadata settings on every refresh the
/// reader runs in (`MetadataService.cs:1381-1384`), and
/// `UseFileCreationTimeForDateAdded` only picks the resolver's date for a
/// new item (`ResolverHelper.SetDateCreated`) and the re-date of a changed
/// file (`MetadataService.cs:383-388`), so neither setting holds it back.
#[rstest::rstest]
#[case::file_creation_by_scan(true, false)]
#[case::file_creation_by_watcher(true, true)]
#[case::date_scanned_by_scan(false, false)]
#[case::date_scanned_by_watcher(false, true)]
#[tokio::test(flavor = "multi_thread")]
async fn an_edited_nfo_dateadded_redates_a_stored_episode(
    #[case] file_creation: bool,
    #[case] by_watcher: bool,
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), file_creation).await;
    let episode = fx.add_episode(1);
    fx.scan().await;
    fx.set_date_created(BaseItemKind::Episode, &episode, "2020-01-01T00:00:00Z")
        .await;

    let nfo = episode.with_extension("nfo");
    std::fs::write(
        &nfo,
        "<episodedetails><title>One</title>\
         <dateadded>2015-05-05 10:00:00</dateadded></episodedetails>",
    )
    .expect("nfo");
    // Written well after the item's last save (`BaseNfoProvider.HasChanged`
    // allows a minute for its own writes).
    std::fs::File::options()
        .write(true)
        .open(&nfo)
        .expect("open")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3_600))
        .expect("mtime");
    if by_watcher {
        fx.report(&nfo).await;
    } else {
        fx.scan().await;
    }
    let row = fx.episode(&episode).await;
    assert_eq!(row.date_created, Some(parse("2015-05-05T10:00:00Z")));
    assert_eq!(fx.series_last_media_added().await, row.date_created);
}

/// A new episode whose NFO carries `<dateadded>` is dated by it on its first
/// scan or watcher report, under either date-added rule: the resolver's
/// date (the file's creation, or the moment of detection) is only where the
/// merge starts, and the reader's `DateCreated` replaces it
/// (`MetadataService.cs:1381-1384`).
#[rstest::rstest]
#[case::file_creation_by_scan(true, false)]
#[case::file_creation_by_watcher(true, true)]
#[case::date_scanned_by_scan(false, false)]
#[case::date_scanned_by_watcher(false, true)]
#[tokio::test(flavor = "multi_thread")]
async fn a_new_episodes_nfo_dateadded_dates_it(
    #[case] file_creation: bool,
    #[case] by_watcher: bool,
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), file_creation).await;
    fx.scan().await;
    let episode = fx.add_episode(1);
    std::fs::write(
        episode.with_extension("nfo"),
        "<episodedetails><title>One</title>\
         <dateadded>2015-05-05 10:00:00</dateadded></episodedetails>",
    )
    .expect("nfo");
    if by_watcher {
        fx.report(&episode).await;
    } else {
        fx.scan().await;
    }
    let row = fx.episode(&episode).await;
    assert_eq!(row.date_created, Some(parse("2015-05-05T10:00:00Z")));
    assert_eq!(fx.series_last_media_added().await, row.date_created);
}

/// A JPEG whose EXIF `DateTime` is 2015-05-05 10:00:00.
fn jpeg_taken_2015() -> Vec<u8> {
    let mut plain = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(4, 4, image::Rgb([200, 30, 30]))
        .write_to(&mut plain, image::ImageFormat::Jpeg)
        .expect("encode");
    let plain = plain.into_inner();
    // APP1 "Exif": a big-endian TIFF whose IFD0 holds one ASCII entry,
    // `DateTime` (0x0132), its 20 bytes right after the IFD.
    let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
    tiff.extend_from_slice(&1u16.to_be_bytes());
    tiff.extend_from_slice(&0x0132u16.to_be_bytes());
    tiff.extend_from_slice(&2u16.to_be_bytes());
    tiff.extend_from_slice(&20u32.to_be_bytes());
    tiff.extend_from_slice(&26u32.to_be_bytes());
    tiff.extend_from_slice(&0u32.to_be_bytes());
    tiff.extend_from_slice(b"2015:05:05 10:00:00\0");
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut jpeg = plain[..2].to_vec();
    jpeg.extend_from_slice(&[0xFF, 0xE1]);
    jpeg.extend_from_slice(&u16::try_from(app1.len() + 2).expect("short").to_be_bytes());
    jpeg.extend_from_slice(&app1);
    jpeg.extend_from_slice(&plain[2..]);
    jpeg
}

/// A photo's EXIF date is its date added, on its first scan and again
/// whenever its reader runs on the stored photo (its file changed) — under
/// "Use date scanned" too, where the change itself re-dates nothing
/// (`PhotoProvider` always writes `DateCreated` from `DateTaken`).
#[tokio::test(flavor = "multi_thread")]
async fn a_photos_exif_date_is_reapplied_when_its_reader_runs() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("photos");
    std::fs::create_dir_all(&media).expect("mkdir");
    let photo = media.join("shot.jpg");
    std::fs::write(&photo, jpeg_taken_2015()).expect("photo");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Photos",
        Some(CollectionTypeOptions::homevideos),
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
    let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> =
        Arc::new(ferrofin_drawing::ImageProcessor::new(
            Arc::new(ferrofin_drawing::ImageCrateEncoder::new()),
            tmp.path().join("cache"),
        ));
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::clone(&items))
        .with_image_processor(processor)
        .with_progress_every(0)
        .with_metadata_configuration(|| MetadataConfiguration {
            use_file_creation_time_for_date_added: false,
        });
    let id = derive_item_id(BaseItemKind::Photo, &photo.to_string_lossy()).expect("id");
    let date_created = async || {
        items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row")
            .date_created
    };
    let taken = Some(parse("2015-05-05T10:00:00Z"));

    scanner.scan_all().await.expect("scan");
    assert_eq!(
        date_created().await,
        taken,
        "a new photo is dated by its EXIF"
    );

    sqlx::query(r#"UPDATE "BaseItems" SET "DateCreated" = ?1 WHERE "Id" = ?2"#)
        .bind(datetime_to_db(parse("2020-01-01T00:00:00Z")))
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("update");
    scanner.scan_all().await.expect("rescan");
    assert_eq!(
        date_created().await,
        Some(parse("2020-01-01T00:00:00Z")),
        "an unchanged photo's reader does not run"
    );

    set_mtime(&photo, "2002-02-02T00:00:00Z");
    scanner.scan_all().await.expect("rescan");
    assert_eq!(
        date_created().await,
        taken,
        "the changed photo's EXIF date is written again"
    );
}
