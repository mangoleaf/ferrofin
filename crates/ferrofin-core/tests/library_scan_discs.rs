//! Disc rips and multi-disc sets as the movie walk plans them
//! (PLAN_ITEM_FILE_DELETION step 12 S7): `FindMovie`'s folder-rip search
//! (`MovieResolver.cs:420-470`) makes a folder holding a DVD's `VIDEO_TS`
//! folder or `VIDEO_TS.IFO` file, or a Blu-ray's `BDMV` folder, ONE video
//! whose path is the folder; `GetMultiDiscMovie` (`:511-582`) makes a
//! folder of disc folders that stack one video, its path the first disc,
//! the others its `AdditionalParts`. A rip is a `Video` like any — never a
//! folder (`BaseItem.IsFolder`, `BaseItem.cs:803`) — though its path is a
//! directory, which has no `DateModified` (owner decision D2).
//!
//! Upstream ships no resolver test for discs; the stacking cases are
//! `StackTests.TestDirectories`/`TestMultiDiscs` (in `ferrofin-naming`), and
//! the 12.1 fixture's `Mike Portnoy - In Constant Motion/D1..D3` — three DVD
//! `Video`s that do not stack — is a case here.

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemPersistenceService as _;
use uuid::Uuid;

/// One library over a temporary media folder, scanned by a real scanner.
struct Fixture {
    _tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    store: Arc<FerrofinItemPersistenceService>,
    vf: Arc<dyn VirtualFolderManager>,
    scanner: LibraryScanner,
}

impl Fixture {
    /// A library of `kind` holding `files` (empty files).
    async fn new(files: &[&str], kind: CollectionTypeOptions) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        for file in files {
            let path = media.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let store = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
                .with_item_store(store.clone()),
        );
        vf.add_virtual_folder(
            "Library",
            Some(kind),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
        let repo = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner = LibraryScanner::new(
            vf.clone(),
            Arc::new(FerrofinFileSystem::new()),
            store.clone(),
        )
        .with_items(repo);
        Self {
            _tmp: tmp,
            media,
            db,
            store,
            vf,
            scanner,
        }
    }

    fn path(&self, relative: &str) -> String {
        self.media.join(relative).to_string_lossy().into_owned()
    }

    fn id(&self, kind: BaseItemKind, relative: &str) -> String {
        guid_to_db(derive_item_id(kind, &self.path(relative)).unwrap())
    }

    /// The one row stored at `relative`.
    async fn row(&self, relative: &str) -> BaseItemEntity {
        sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Path" = ?"#)
            .bind(self.path(relative))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    /// How many rows are stored at `relative`.
    async fn rows_at(&self, relative: &str) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Path" = ?"#)
            .bind(self.path(relative))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    /// The media rows below the library (no folder, no library row): kind
    /// and path relative to the library, sorted.
    async fn media_rows(&self) -> Vec<(String, String)> {
        let prefix = format!("{}/", self.media.to_string_lossy());
        let mut rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "Type", "Path" FROM "BaseItems"
               WHERE "IsFolder" = 0 AND "Path" LIKE ?1 || '%'"#,
        )
        .bind(&prefix)
        .fetch_all(self.db.pool())
        .await
        .unwrap();
        for row in &mut rows {
            row.0 = row.0.rsplit('.').next().unwrap().to_owned();
            row.1 = row.1.strip_prefix(&prefix).unwrap().to_owned();
        }
        rows.sort();
        rows
    }

    /// Whether `user` played the item `id`.
    async fn played(&self, id: &str, user: Uuid) -> bool {
        sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1 AND "UserId" = ?2
               AND "Played" = 1"#,
        )
        .bind(id)
        .bind(guid_to_db(user))
        .fetch_one(self.db.pool())
        .await
        .unwrap()
            == 1
    }
}

fn data(row: &BaseItemEntity) -> serde_json::Value {
    serde_json::from_str(row.data.as_deref().unwrap_or("{}")).unwrap()
}

async fn seed_user(db: &Database) -> Uuid {
    let user = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, ?2, upper(?2))"#,
    )
    .bind(guid_to_db(user))
    .bind(user.simple().to_string())
    .execute(db.writer())
    .await
    .unwrap();
    user
}

/// `user` played the item `id`.
async fn play(db: &Database, id: &str, user: Uuid) {
    sqlx::query(
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
           "PlayCount", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, ?1, 0, 1, 0, 1)"#,
    )
    .bind(id)
    .bind(guid_to_db(user))
    .execute(db.writer())
    .await
    .unwrap();
}

/// Counts the writes to `BaseItems` from here on.
async fn count_item_writes(db: &Database) {
    sqlx::query(r#"CREATE TABLE "TestItemWrites" ("N" INTEGER NOT NULL)"#)
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "TestItemWrites" VALUES (0)"#)
        .execute(db.writer())
        .await
        .unwrap();
    for event in ["INSERT", "UPDATE", "DELETE"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"CREATE TRIGGER "TestItemWrites_{event}" AFTER {event} ON "BaseItems"
               BEGIN UPDATE "TestItemWrites" SET "N" = "N" + 1; END"#
        )))
        .execute(db.writer())
        .await
        .unwrap();
    }
}

async fn item_writes(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT "N" FROM "TestItemWrites""#)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// `IsDvdFile` (`BaseVideoResolver.cs:274-277`): a `VIDEO_TS.IFO` file
/// directly in a folder makes the folder ONE DVD — not a movie per `.vob`
/// beside it. The rip is a `Video` row, not a folder: `IsFolder = 0`, dated
/// as a directory (no `DateModified`, no `Size`), named after its folder.
#[tokio::test]
async fn a_video_ts_ifo_file_makes_its_folder_one_dvd() {
    let f = Fixture::new(
        &[
            "Alien (1979)/VIDEO_TS.IFO",
            "Alien (1979)/VTS_01_1.VOB",
            "Alien (1979)/VTS_01_2.VOB",
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();

    assert_eq!(
        f.media_rows().await,
        vec![("Movie".to_owned(), "Alien (1979)".to_owned())]
    );
    let rip = f.row("Alien (1979)").await;
    assert_eq!(rip.id, f.id(BaseItemKind::Movie, "Alien (1979)"));
    assert!(!rip.is_folder, "a rip is a Video, never a folder");
    assert_eq!(rip.date_modified, None, "a directory has no DateModified");
    assert_eq!(rip.size, None, "a directory has no Size");
    assert_eq!(rip.name.as_deref(), Some("Alien (1979)"));
    assert_eq!(rip.production_year, Some(1979));
    assert_eq!(data(&rip)["VideoType"], "Dvd");
    assert_eq!(data(&rip)["AdditionalParts"], serde_json::json!([]));
}

/// Stores the row an older scan made of the file at `relative` — a video
/// of its own (`kind`) — shaped from `like`; returns its id.
async fn older_row(
    f: &Fixture,
    like: &BaseItemEntity,
    kind: BaseItemKind,
    relative: &str,
) -> String {
    let mut older = like.clone();
    older.id = f.id(kind, relative);
    ferrofin_core::item_type_lookup::stored_type_name(kind)
        .unwrap()
        .clone_into(&mut older.type_);
    older.path = Some(f.path(relative));
    older.name = Some(relative.rsplit('/').next().unwrap().to_owned());
    older.sort_name = None;
    older.presentation_unique_key = None;
    older.owner_id = None;
    older.is_folder = false;
    older.data = Some(r#"{"VideoType":"VideoFile"}"#.to_owned());
    f.store.save_items(&[older.clone()]).await.unwrap();
    older.id
}

/// The rows an older scan planned inside a rip — a movie per `.vob` — are
/// the rip itself: they fold into it, their user data with it (Ferrofin
/// keeps the watch history upstream drops, D9b's principle), and are gone.
#[tokio::test]
async fn a_rips_older_per_file_rows_fold_into_it() {
    const VOB: &str = "Alien (1979)/VTS_01_1.VOB";
    let f = Fixture::new(
        &["Alien (1979)/VIDEO_TS.IFO", VOB],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let rip = f.row("Alien (1979)").await;
    let older = older_row(&f, &rip, BaseItemKind::Movie, VOB).await;
    let user = seed_user(&f.db).await;
    play(&f.db, &older, user).await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(VOB).await, 0);
    assert_eq!(f.rows_at("Alien (1979)").await, 1);
    assert!(
        f.played(&rip.id, user).await,
        "the .vob row's user data moved"
    );
    let quiet = f.scanner.scan_all().await.unwrap();
    assert_eq!((quiet.created, quiet.updated), (0, 0), "{quiet:?}");
}

/// Where the rip is not stored yet, its older per-file row waits — out of
/// the prune — and folds on the next scan.
#[tokio::test]
async fn a_rips_older_row_waits_for_the_rip() {
    const VOB: &str = "Alien (1979)/VIDEO_TS/VTS_01_1.VOB";
    let f = Fixture::new(&[VOB], CollectionTypeOptions::movies).await;
    f.scanner.scan_all().await.unwrap();
    let rip = f.row("Alien (1979)").await;
    let older = older_row(&f, &rip, BaseItemKind::Movie, VOB).await;
    f.store
        .delete_items(&[Uuid::parse_str(&rip.id).unwrap()])
        .await
        .unwrap();
    let user = seed_user(&f.db).await;
    play(&f.db, &older, user).await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(VOB).await, 1, "kept until it can fold");
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(VOB).await, 0);
    assert!(f.played(&rip.id, user).await);
}

/// A rip's folder that holds other media — a subfolder no extras name
/// marks, a loose video no extra rule gives the rip — swallows it: nothing
/// below a rip resolves, so their stored rows are pruned (as upstream), and
/// one warning names them first.
#[tokio::test]
async fn what_a_rip_swallows_is_pruned_with_a_warning() {
    const LOOSE: &str = "Alien (1979)/Director Commentary.mkv";
    const NESTED: &str = "Alien (1979)/Bonus/Making Of.mkv";
    let f = Fixture::new(
        &["Alien (1979)/VIDEO_TS/VTS_01_1.VOB", LOOSE, NESTED],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(LOOSE).await, 0, "nothing below a rip resolves");
    let rip = f.row("Alien (1979)").await;
    for path in [LOOSE, NESTED] {
        older_row(&f, &rip, BaseItemKind::Movie, path).await;
    }

    let logs = Logs::default();
    {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        f.scanner.scan_all().await.unwrap();
    }
    assert_eq!(f.rows_at(LOOSE).await, 0);
    assert_eq!(f.rows_at(NESTED).await, 0);
    assert_eq!(f.rows_at("Alien (1979)").await, 1);
    let text = logs.text();
    let warnings: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("WARN") && l.contains("holds other media"))
        .collect();
    assert_eq!(warnings.len(), 1, "{text}");
    assert!(warnings[0].contains("swallowed=2"), "{}", warnings[0]);
    assert!(
        warnings[0].contains("Director Commentary.mkv"),
        "{}",
        warnings[0]
    );
}

/// A log sink for one test.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A row inside a rip that a library this scan does not plan also holds —
/// its location is the rip's disc folder — is never folded into the rip
/// (`held_by_unscanned`, as S2's re-key guards) nor pruned with what the
/// rip excludes: it and its user data are that library's.
#[tokio::test]
async fn a_row_another_library_holds_is_never_folded() {
    const VOB: &str = "Alien (1979)/VIDEO_TS/VTS_01_1.VOB";
    let f = Fixture::new(&[VOB], CollectionTypeOptions::movies).await;
    f.scanner.scan_all().await.unwrap();
    let movies = f.vf.get_virtual_folders().await.unwrap()[0]
        .item_id
        .clone()
        .unwrap();
    f.vf.add_virtual_folder(
        "Clips",
        Some(CollectionTypeOptions::homevideos),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: f.path("Alien (1979)/VIDEO_TS"),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .unwrap();
    let rip = f.row("Alien (1979)").await;
    let older = older_row(&f, &rip, BaseItemKind::Movie, VOB).await;
    let user = seed_user(&f.db).await;
    play(&f.db, &older, user).await;

    f.scanner
        .scan(Some(Uuid::parse_str(&movies).unwrap()))
        .await
        .unwrap();
    assert!(
        !f.played(&rip.id, user).await,
        "nothing folded into the rip"
    );
    assert_eq!(f.rows_at(VOB).await, 1, "the other library's row survives");
    assert!(f.played(&older, user).await, "with its user data");
}

/// A rip owns the extras of its folder (`FindExtras` over its
/// `ContainingFolderPath`, the rip's own folder, `Video.cs:211-216`): a
/// `-trailer` file beside the disc and an extras folder's files.
#[tokio::test]
async fn a_rip_owns_the_extras_of_its_folder() {
    let f = Fixture::new(
        &[
            "Avatar (2009)/BDMV/STREAM/00001.m2ts",
            "Avatar (2009)/Avatar (2009)-trailer.mkv",
            "Avatar (2009)/featurettes/Making Of.mkv",
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let rip = f.row("Avatar (2009)").await;
    assert_eq!(data(&rip)["VideoType"], "BluRay");
    for extra in [
        "Avatar (2009)/Avatar (2009)-trailer.mkv",
        "Avatar (2009)/featurettes/Making Of.mkv",
    ] {
        let row = f.row(extra).await;
        assert_eq!(row.owner_id.as_ref(), Some(&rip.id), "{extra}");
        assert!(row.extra_type.is_some(), "{extra}");
    }
    assert_eq!(f.rows_at("Avatar (2009)/BDMV/STREAM/00001.m2ts").await, 0);
}

/// `GetMultiDiscMovie` (`MovieResolver.cs:511-582`, upstream
/// `StackTests.TestMultiDiscs`' folders): a folder whose only content is
/// two DVD folders that stack is ONE movie — its path the first disc
/// folder, the second its `AdditionalParts`, named after the stack and
/// dated from it. The second disc gets no row (`Video.
/// RefreshMetadataForOwnedVideo` creates a part only where `File.Exists`,
/// never for a directory), so `GET /Videos/{id}/AdditionalParts` lists
/// nothing, as upstream. A rescan writes nothing and keeps the user data.
#[tokio::test]
async fn a_multi_disc_set_is_one_movie_whose_discs_have_no_rows() {
    use ferrofin_traits::library::LibraryManager as _;
    let f = Fixture::new(
        &[
            &format!("{FIRST_DISC}/VIDEO_TS/VTS_01_1.VOB"),
            &format!("{SECOND_DISC}/VIDEO_TS/VTS_01_1.VOB"),
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();

    assert_eq!(
        f.media_rows().await,
        vec![("Movie".to_owned(), FIRST_DISC.to_owned())]
    );
    let movie = f.row(FIRST_DISC).await;
    assert_eq!(movie.id, f.id(BaseItemKind::Movie, FIRST_DISC));
    assert_eq!(movie.name.as_deref(), Some("The Sound of Music (1965)"));
    assert_eq!(movie.production_year, Some(1965));
    assert!(!movie.is_folder);
    assert!(!movie.is_in_mixed_folder);
    assert!(movie.owner_id.is_none());
    assert_eq!(movie.date_modified, None);
    assert_eq!(data(&movie)["VideoType"], "Dvd");
    assert_eq!(
        data(&movie)["AdditionalParts"],
        serde_json::json!([f.path(SECOND_DISC)])
    );
    assert_eq!(f.rows_at(SECOND_DISC).await, 0);
    let manager = ferrofin_core::FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(
            f.db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        Arc::new(ferrofin_core::FerrofinItemCountService::new(f.db.clone())),
        f.store.clone(),
        Arc::new(ferrofin_core::FerrofinPeopleRepository::new(f.db.clone())),
    );
    assert!(
        manager
            .get_additional_parts(&movie, None)
            .await
            .unwrap()
            .is_empty()
    );

    let user = seed_user(&f.db).await;
    play(&f.db, &movie.id, user).await;
    count_item_writes(&f.db).await;
    let rescan = f.scanner.scan_all().await.unwrap();
    assert_eq!((rescan.created, rescan.updated), (0, 0), "{rescan:?}");
    assert_eq!(item_writes(&f.db).await, 0);
    assert!(f.played(&movie.id, user).await);
}

const FIRST_DISC: &str = "The Sound of Music/The Sound of Music (1965) (Disc 01)";
const SECOND_DISC: &str = "The Sound of Music/The Sound of Music (1965) (Disc 02)";

/// The row an older scan stored at a set's second disc (a rip `Movie` of
/// its own, stored as a folder) is the same movie: it folds into the set's
/// movie, its user data with it, and is gone; the scan after is quiet.
/// Upstream drops that row and its user data; Ferrofin keeps the user data
/// (D9b's principle).
#[tokio::test]
async fn an_older_disc_row_folds_into_the_set_keeping_user_data() {
    let f = Fixture::new(
        &[
            &format!("{FIRST_DISC}/VIDEO_TS/VTS_01_1.VOB"),
            &format!("{SECOND_DISC}/VIDEO_TS/VTS_01_1.VOB"),
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let movie = f.row(FIRST_DISC).await;
    let older = older_disc_row(&f, &movie).await;
    let user = seed_user(&f.db).await;
    play(&f.db, &older, user).await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(SECOND_DISC).await, 0, "the older disc row folded");
    assert!(f.played(&movie.id, user).await, "its user data moved");
    let quiet = f.scanner.scan_all().await.unwrap();
    assert_eq!((quiet.created, quiet.updated), (0, 0), "{quiet:?}");
}

/// Where the set's movie is not stored yet, the older disc row waits — out
/// of the prune — and folds on the next scan, once the movie is.
#[tokio::test]
async fn an_older_disc_row_waits_for_its_movie() {
    let f = Fixture::new(
        &[
            &format!("{FIRST_DISC}/VIDEO_TS/VTS_01_1.VOB"),
            &format!("{SECOND_DISC}/VIDEO_TS/VTS_01_1.VOB"),
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let movie = f.row(FIRST_DISC).await;
    let older = older_disc_row(&f, &movie).await;
    f.store
        .delete_items(&[Uuid::parse_str(&movie.id).unwrap()])
        .await
        .unwrap();
    let user = seed_user(&f.db).await;
    play(&f.db, &older, user).await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(SECOND_DISC).await, 1, "kept until it can fold");
    assert!(f.played(&older, user).await);
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(SECOND_DISC).await, 0);
    assert!(f.played(&movie.id, user).await);
}

/// Stores the row an older scan made of the set's second disc — a rip
/// `Movie` of its own, `IsFolder = 1` — beside `movie`; returns its id.
async fn older_disc_row(f: &Fixture, movie: &BaseItemEntity) -> String {
    let mut older = movie.clone();
    older.id = f.id(BaseItemKind::Movie, SECOND_DISC);
    older.path = Some(f.path(SECOND_DISC));
    older.name = Some("The Sound of Music (1965) (Disc 02)".to_owned());
    older.sort_name = None;
    older.presentation_unique_key = None;
    older.is_folder = true;
    older.data = Some(r#"{"VideoType":"Dvd","AdditionalParts":[]}"#.to_owned());
    f.store.save_items(&[older.clone()]).await.unwrap();
    older.id
}

/// The 12.1 fixture's `Mike Portnoy - In Constant Motion/D1..D3`: three DVD
/// folders whose names no stacking rule takes (`D1` has no part type), so
/// `GetMultiDiscMovie` finds no stack and each disc is a `Video` of its own
/// — as Jellyfin stored them, `IsFolder = 0`. Likewise `CD1`/`CD2` with no
/// title before them: the rule needs a separator ahead of the part type.
#[rstest::rstest]
#[case::portnoy(
    "Mike Portnoy - In Constant Motion",
    &["D1", "D2", "D3"],
    CollectionTypeOptions::homevideos,
    "Video"
)]
#[case::bare_cd_folders("Movie (2004)", &["CD1", "CD2"], CollectionTypeOptions::movies, "Movie")]
#[tokio::test]
async fn discs_that_do_not_stack_stay_videos_of_their_own(
    #[case] set: &str,
    #[case] discs: &[&str],
    #[case] library: CollectionTypeOptions,
    #[case] kind: &str,
) {
    let files: Vec<String> = discs
        .iter()
        .map(|disc| format!("{set}/{disc}/VIDEO_TS/VTS_01_1.VOB"))
        .collect();
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let f = Fixture::new(&refs, library).await;
    f.scanner.scan_all().await.unwrap();
    let expected: Vec<(String, String)> = discs
        .iter()
        .map(|disc| (kind.to_owned(), format!("{set}/{disc}")))
        .collect();
    assert_eq!(f.media_rows().await, expected);
    for disc in discs {
        let row = f.row(&format!("{set}/{disc}")).await;
        assert!(!row.is_folder);
        assert!(row.owner_id.is_none());
        assert_eq!(row.name.as_deref(), Some(*disc));
        assert_eq!(data(&row)["VideoType"], "Dvd");
        assert_eq!(data(&row)["AdditionalParts"], serde_json::json!([]));
    }
}

/// Discs of two kinds never make a set (`videoTypes.Distinct().Count() > 1`,
/// `MovieResolver.cs:550-553`): each is a rip of its own.
#[tokio::test]
async fn discs_of_two_kinds_are_no_set() {
    const FIRST: &str = "Heat (1995)/Heat (1995) - Disc 1";
    const SECOND: &str = "Heat (1995)/Heat (1995) - Disc 2";
    let f = Fixture::new(
        &[
            &format!("{FIRST}/VIDEO_TS/VTS_01_1.VOB"),
            &format!("{SECOND}/BDMV/index.bdmv"),
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(
        f.media_rows().await,
        vec![
            ("Movie".to_owned(), FIRST.to_owned()),
            ("Movie".to_owned(), SECOND.to_owned()),
        ]
    );
    assert_eq!(data(&f.row(FIRST).await)["VideoType"], "Dvd");
    assert_eq!(data(&f.row(SECOND).await)["VideoType"], "BluRay");
}

/// A rip an older Ferrofin scan stored `IsFolder = 1` is rewritten once to
/// the `Video` shape upstream stores (`IsFolder = 0`), keeping its id and
/// its user data; the scan after writes nothing.
#[tokio::test]
async fn a_rip_stored_as_a_folder_is_rewritten_once() {
    const RIP: &str = "Alien (1979)";
    let f = Fixture::new(
        &["Alien (1979)/VIDEO_TS/VTS_01_1.VOB"],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let id = f.row(RIP).await.id;
    sqlx::query(r#"UPDATE "BaseItems" SET "IsFolder" = 1 WHERE "Id" = ?1"#)
        .bind(&id)
        .execute(f.db.writer())
        .await
        .unwrap();
    let user = seed_user(&f.db).await;
    play(&f.db, &id, user).await;

    let rewrite = f.scanner.scan_all().await.unwrap();
    assert_eq!(rewrite.updated, 1, "{rewrite:?}");
    let rip = f.row(RIP).await;
    assert_eq!(rip.id, id);
    assert!(!rip.is_folder);
    assert_eq!(rip.date_modified, None);
    assert!(f.played(&id, user).await);

    count_item_writes(&f.db).await;
    let quiet = f.scanner.scan_all().await.unwrap();
    assert_eq!((quiet.created, quiet.updated), (0, 0), "{quiet:?}");
    assert_eq!(item_writes(&f.db).await, 0);
}

/// A rip's stored `DateModified` (a stamp an older scan or a file-style
/// save left) is cleared by the scan's save, as a folder's is (D2): its
/// path is a directory though the row is no folder.
#[tokio::test]
async fn a_rips_stale_date_modified_is_cleared() {
    const RIP: &str = "Alien (1979)";
    let f = Fixture::new(
        &["Alien (1979)/VIDEO_TS/VTS_01_1.VOB"],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let id = f.row(RIP).await.id;
    sqlx::query(r#"UPDATE "BaseItems" SET "DateModified" = '2001-09-09 01:46:40' WHERE "Id" = ?1"#)
        .bind(&id)
        .execute(f.db.writer())
        .await
        .unwrap();
    let cleared = f.scanner.scan_all().await.unwrap();
    assert_eq!(cleared.updated, 1, "{cleared:?}");
    assert_eq!(f.row(RIP).await.date_modified, None);
    let quiet = f.scanner.scan_all().await.unwrap();
    assert_eq!(quiet.updated, 0, "{quiet:?}");
}

/// `FindExtras` resolves a multi-disc movie's owner from its path, the first
/// disc folder, as a folder (`LibraryManager.cs:3446-3447`): that folder is
/// no extra's parent (`ExtraResolver.cs:71,85-91`), so an extra beside the
/// discs is the movie's only when named after it — its first disc's name,
/// or its title and year — and the set's `trailers/` folder holds none.
#[tokio::test]
async fn a_sets_extras_must_be_named_after_it() {
    const NAMED: &str = "The Sound of Music/The Sound of Music (1965)-trailer.mkv";
    const UNNAMED: &str = "The Sound of Music/trailers/Teaser.mkv";
    let f = Fixture::new(
        &[
            &format!("{FIRST_DISC}/VIDEO_TS/VTS_01_1.VOB"),
            &format!("{SECOND_DISC}/VIDEO_TS/VTS_01_1.VOB"),
            NAMED,
            UNNAMED,
        ],
        CollectionTypeOptions::movies,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let movie = f.row(FIRST_DISC).await;
    let named = f.row(NAMED).await;
    assert_eq!(named.owner_id.as_ref(), Some(&movie.id));
    assert!(named.extra_type.is_some());
    assert_eq!(f.rows_at(UNNAMED).await, 0, "no extra of the set's");
}
