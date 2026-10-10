//! Where the scanner's row reshaping (PLAN_ITEM_FILE_DELETION: folds,
//! rip/set exclusions, released versions) meets user-data retention
//! (`docs/reviews/user-data-retention.md`): every path that removes a row
//! keeps its users' history exactly once.
//!
//! - A fold (`merge_one`) MOVES the loser's history onto the kept row —
//!   upstream's best-of-duplicates rule per (user, key) — so nothing is
//!   detached to the placeholder or snapshotted: a later recovery pass can
//!   never resurrect a second copy.
//! - A row a rip swallows is pruned through `delete_items`, which detaches
//!   its history; the file coming back at its path recovers it.
//! - A part or version released from a pruned owner keeps its row, and so
//!   its history in place; only the pruned owner's history is detached.

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id, stored_type_name};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::PLACEHOLDER_ITEM_ID;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemPersistenceService as _;
use uuid::Uuid;

/// One movie library over a temporary media folder, scanned by a real
/// scanner.
struct Fixture {
    _tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    store: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
}

impl Fixture {
    async fn movies(files: &[&str]) -> Self {
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
            Some(CollectionTypeOptions::movies),
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
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), store.clone())
            .with_items(repo);
        Self {
            _tmp: tmp,
            media,
            db,
            store,
            scanner,
        }
    }

    fn path(&self, relative: &str) -> String {
        self.media.join(relative).to_string_lossy().into_owned()
    }

    fn id(&self, kind: BaseItemKind, relative: &str) -> Uuid {
        derive_item_id(kind, &self.path(relative)).unwrap()
    }

    async fn row(&self, relative: &str) -> BaseItemEntity {
        sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Path" = ?"#)
            .bind(self.path(relative))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn rows_at(&self, relative: &str) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Path" = ?"#)
            .bind(self.path(relative))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    /// Stores the row an older scan made of the file at `relative` — a
    /// `Movie` of its own — shaped from `like`; returns its id.
    async fn older_movie(&self, like: &BaseItemEntity, relative: &str) -> Uuid {
        let id = self.id(BaseItemKind::Movie, relative);
        let mut older = like.clone();
        older.id = guid_to_db(id);
        stored_type_name(BaseItemKind::Movie)
            .unwrap()
            .clone_into(&mut older.type_);
        older.path = Some(self.path(relative));
        older.name = Some(relative.rsplit('/').next().unwrap().to_owned());
        older.sort_name = None;
        older.presentation_unique_key = None;
        older.owner_id = None;
        older.is_folder = false;
        older.data = Some(r#"{"VideoType":"VideoFile"}"#.to_owned());
        self.store.save_items(&[older]).await.unwrap();
        id
    }

    /// Every user-data row of `user`: (item, key, play count, ticks,
    /// retention date), sorted.
    async fn user_data(&self, user: Uuid) -> Vec<UserDataRow> {
        sqlx::query_as(
            r#"SELECT "ItemId", "CustomDataKey", "PlayCount", "PlaybackPositionTicks",
                      "RetentionDate"
               FROM "UserData" WHERE "UserId" = ?1 ORDER BY "ItemId", "CustomDataKey""#,
        )
        .bind(guid_to_db(user))
        .fetch_all(self.db.pool())
        .await
        .unwrap()
    }

    /// The retention snapshots taken of item `id`.
    async fn snapshots_of(&self, id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "FerrofinUserDataRetentionSnapshots" WHERE "ItemId" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }

    async fn all_snapshots(&self) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinUserDataRetentionSnapshots""#)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

/// (item, key, play count, ticks, retention date).
type UserDataRow = (String, String, i64, i64, Option<String>);

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

/// `user` played item `id` `count` times, last on `date`, stopping at
/// `ticks` — under the item's own guid key, as `UserDataManager` keys it.
async fn play(db: &Database, id: Uuid, user: Uuid, count: i64, ticks: i64, date: &str) {
    sqlx::query(
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
           "PlayCount", "PlaybackPositionTicks", "Played", "LastPlayedDate")
           VALUES (?1, ?2, ?3, 0, ?4, ?5, 1, ?6)"#,
    )
    .bind(guid_to_db(id))
    .bind(guid_to_db(user))
    .bind(id.to_string())
    .bind(count)
    .bind(ticks)
    .bind(format!("{date} 00:00:00.0000000"))
    .execute(db.writer())
    .await
    .unwrap();
}

/// A rip's older per-file row folds into the rip (`merge_one`): each user's
/// history lands on the rip exactly once — the loser's where it was played
/// later, the rip's where it was, the loser's alone where the rip had none
/// — and nothing is detached or snapshotted, so the scan's recovery pass
/// (and every later one) has no second copy to bring back.
#[tokio::test]
async fn a_folds_loser_history_moves_once_and_is_never_detached() {
    const VOB: &str = "Alien (1979)/VTS_01_1.VOB";
    let f = Fixture::movies(&["Alien (1979)/VIDEO_TS.IFO", VOB]).await;
    f.scanner.scan_all().await.unwrap();
    let rip = f.row("Alien (1979)").await;
    let rip_id = Uuid::parse_str(&rip.id).unwrap();
    let loser = f.older_movie(&rip, VOB).await;
    // Loser played later: its history wins.
    let later = seed_user(&f.db).await;
    play(&f.db, rip_id, later, 1, 10, "2026-01-01").await;
    play(&f.db, loser, later, 5, 42, "2026-06-01").await;
    // Rip played later: the rip's stays.
    let earlier = seed_user(&f.db).await;
    play(&f.db, rip_id, earlier, 7, 70, "2026-09-01").await;
    play(&f.db, loser, earlier, 2, 20, "2026-02-01").await;
    // Only the loser was played: it moves.
    let only = seed_user(&f.db).await;
    play(&f.db, loser, only, 3, 33, "2026-03-01").await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(VOB).await, 0, "the older row folded");
    let on_rip = |count, ticks| vec![(rip.id.clone(), rip_id.to_string(), count, ticks, None)];
    assert_eq!(f.user_data(later).await, on_rip(5, 42));
    assert_eq!(f.user_data(earlier).await, on_rip(7, 70));
    assert_eq!(f.user_data(only).await, on_rip(3, 33));
    assert_eq!(f.all_snapshots().await, 0, "a fold detaches nothing");

    // Quiet rescans neither duplicate nor lose it.
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.user_data(later).await, on_rip(5, 42));
    assert_eq!(f.user_data(only).await, on_rip(3, 33));
}

/// A row a rip swallows (a loose video beside its `VIDEO_TS`) is pruned
/// through `delete_items`: its history is detached to the placeholder with
/// a retention date and a source snapshot. When the rip goes and the file
/// resolves at its path again, the next scan recovers it onto the same id
/// and consumes the snapshot.
#[tokio::test]
async fn a_rip_swallowed_rows_history_is_detached_and_recovered() {
    const LOOSE: &str = "Alien (1979)/Director Commentary.mkv";
    let f = Fixture::movies(&["Alien (1979)/VIDEO_TS/VTS_01_1.VOB", LOOSE]).await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(LOOSE).await, 0, "nothing below a rip resolves");
    let rip = f.row("Alien (1979)").await;
    let loose = f.older_movie(&rip, LOOSE).await;
    let user = seed_user(&f.db).await;
    play(&f.db, loose, user, 3, 33, "2026-03-01").await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(LOOSE).await, 0, "the swallowed row is pruned");
    let detached = f.user_data(user).await;
    assert_eq!(detached.len(), 1, "{detached:?}");
    let (item, key, count, ticks, retained) = &detached[0];
    assert_eq!(
        (item.as_str(), key.as_str(), *count, *ticks),
        (PLACEHOLDER_ITEM_ID, loose.to_string().as_str(), 3, 33)
    );
    assert!(retained.is_some(), "detached with a retention date");
    assert_eq!(f.snapshots_of(loose).await, 1);

    // The rip goes; the loose file is a movie of its own at the same path.
    std::fs::remove_dir_all(f.media.join("Alien (1979)").join("VIDEO_TS")).unwrap();
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(LOOSE).await.id, guid_to_db(loose), "the same id");
    assert_eq!(
        f.user_data(user).await,
        vec![(guid_to_db(loose), loose.to_string(), 3, 33, None)],
        "recovered once, the placeholder copy consumed"
    );
    assert_eq!(f.snapshots_of(loose).await, 0, "the snapshot is consumed");
}

/// A part whose owner a scan prunes without planning it is released
/// (`release_owned_versions`), not deleted: its history stays on its row,
/// attached and un-snapshotted, while the pruned owner's own history is
/// detached for recovery. The full rescan after keeps both as they are.
#[tokio::test]
async fn a_released_version_keeps_its_history_attached() {
    let f = Fixture::movies(&["A/Film cd1.mkv", "A/Film cd2.mkv", "B/Other.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("A/Film cd1.mkv").await;
    let primary_id = Uuid::parse_str(&primary.id).unwrap();
    let other = f.row("B/Other.mkv").await;
    let other_id = Uuid::parse_str(&other.id).unwrap();
    // The owned row is elsewhere: no walk of `A` plans it.
    sqlx::query(
        r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#,
    )
    .bind(&other.id)
    .bind(&primary.id)
    .execute(f.db.writer())
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, 0, ?2, 2)"#,
    )
    .bind(&primary.id)
    .bind(&other.id)
    .execute(f.db.writer())
    .await
    .unwrap();
    let user = seed_user(&f.db).await;
    play(&f.db, other_id, user, 4, 44, "2026-04-01").await;
    play(&f.db, primary_id, user, 1, 11, "2026-01-01").await;

    std::fs::remove_file(f.path("A/Film cd1.mkv")).unwrap();
    f.scanner.scan_paths(&[f.path("A")]).await.unwrap();
    assert_eq!(f.rows_at("A/Film cd1.mkv").await, 0, "the owner is pruned");
    assert_eq!(f.row("B/Other.mkv").await.owner_id, None, "released");
    let expected = vec![
        (
            PLACEHOLDER_ITEM_ID.to_owned(),
            primary_id.to_string(),
            1,
            11,
        ),
        (other.id.clone(), other_id.to_string(), 4, 44),
    ];
    let shape = |rows: Vec<UserDataRow>| {
        rows.into_iter()
            .map(|(item, key, count, ticks, retained)| {
                // Only the pruned owner's copy carries a retention date.
                assert_eq!(retained.is_some(), item == PLACEHOLDER_ITEM_ID);
                (item, key, count, ticks)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(f.user_data(user).await), expected);
    assert_eq!(
        f.snapshots_of(other_id).await,
        0,
        "the version kept its row"
    );
    assert_eq!(f.snapshots_of(primary_id).await, 1);

    f.scanner.scan_all().await.unwrap();
    assert_eq!(shape(f.user_data(user).await), expected);
}
