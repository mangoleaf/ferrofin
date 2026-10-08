//! Stacked parts and local alternate versions as the movie walk plans them
//! (PLAN_ITEM_FILE_DELETION step 12 S4): `MovieResolver.ResolveVideos` /
//! `FindMovie` make one item of a stack or a version group
//! (`MovieResolver.cs:287-317,472-494`), the primary carries the others'
//! paths (`AdditionalParts`, `LocalAlternateVersions`), a part is a `Video`
//! its primary owns (`Video.RefreshMetadataForOwnedVideo`, `Video.cs:636-701`)
//! and an alternate a row of the primary's type owned by and pointing at it
//! (`LibraryManager.ResolveAlternateVersion`, `LibraryManager.cs:865-922`).
//!
//! Transliterates upstream's `MovieResolverTests` local-version cases,
//! `ResolveAlternateVersionTests` and `BaseItemTests.
//! GetOwnerIdForExtra_AssignsExtraToItsVersion`; the stacks of a real
//! Jellyfin 12.1 home-video library and the bench's adopted alternates are
//! fixture cases.

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{
    IdDerivation, ItemTypeLookup, derive_item_id, stored_type_name,
};
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
    scanner: LibraryScanner,
}

impl Fixture {
    /// A library of `kind` (none: untyped) holding `files`.
    async fn new(files: &[&str], kind: Option<CollectionTypeOptions>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        for file in files {
            touch(&media, file);
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
            kind,
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

    async fn movies(files: &[&str]) -> Self {
        Self::new(files, Some(CollectionTypeOptions::movies)).await
    }

    fn path(&self, relative: &str) -> String {
        self.media.join(relative).to_string_lossy().into_owned()
    }

    /// The id `kind` derives for the file at `relative`.
    fn id(&self, kind: BaseItemKind, relative: &str) -> Uuid {
        derive_item_id(kind, &self.path(relative)).unwrap()
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

    /// `parent`'s version rows: (child, `ChildType`), in order.
    async fn links(&self, parent: &str) -> Vec<(String, i64)> {
        sqlx::query_as(
            r#"SELECT "ChildId", "ChildType" FROM "LinkedChildren"
               WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
        )
        .bind(parent)
        .fetch_all(self.db.pool())
        .await
        .unwrap()
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

    /// The non-extra rows no one owns: what browse lists.
    async fn titles(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "Type", "Path" FROM "BaseItems" WHERE "OwnerId" IS NULL
               AND "ExtraType" IS NULL AND "IsFolder" = 0 AND "Path" IS NOT NULL"#,
        )
        .fetch_all(self.db.pool())
        .await
        .unwrap();
        rows.sort();
        rows
    }

    /// Replaces the row at `relative` with `shape` of it (which may give it
    /// another id): the row an older scan, or Jellyfin, stored there.
    async fn restore_as(&self, relative: &str, shape: impl FnOnce(&mut BaseItemEntity)) -> String {
        let mut row = self.row(relative).await;
        self.store
            .delete_items(&[Uuid::parse_str(&row.id).unwrap()])
            .await
            .unwrap();
        shape(&mut row);
        self.store.save_items(&[row.clone()]).await.unwrap();
        row.id
    }
}

fn touch(root: &std::path::Path, relative: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"").unwrap();
}

/// A path list of a row's `Data` (`AdditionalParts`, `LocalAlternateVersions`).
fn data_list(row: &BaseItemEntity, key: &str) -> Option<Vec<String>> {
    let data: serde_json::Value = serde_json::from_str(row.data.as_deref()?).ok()?;
    serde_json::from_value(data.get(key)?.clone()).ok()
}

fn kind_of(row: &BaseItemEntity) -> &str {
    row.type_.rsplit('.').next().unwrap()
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

/// Upstream `MovieResolverTests.ResolveMultiple_GivenMoviesCollection_CreatesMovieItems`:
/// a movies library's version group is ONE `Movie` with one local
/// alternate version. Its alternate is a `Movie` too (the primary's type),
/// owned by and pointing at it, keyed by its presentation key, in its
/// folder state, hanging where it does, and linked under it as a local
/// version (`ChildType` 2).
#[tokio::test]
async fn movies_collection_groups_versions_into_one_movie() {
    let f = Fixture::movies(&[
        "Inception (2010)/Inception (2010) - 1080p.mkv",
        "Inception (2010)/Inception (2010) - 720p.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();

    let primary_path = "Inception (2010)/Inception (2010) - 1080p.mkv";
    let alternate_path = "Inception (2010)/Inception (2010) - 720p.mkv";
    assert_eq!(
        f.titles().await,
        vec![(
            stored_type_name(BaseItemKind::Movie).unwrap().to_owned(),
            f.path(primary_path)
        )]
    );
    let primary = f.row(primary_path).await;
    assert_eq!(primary.name.as_deref(), Some("Inception (2010)"));
    assert_eq!(
        data_list(&primary, "LocalAlternateVersions"),
        Some(vec![f.path(alternate_path)])
    );
    assert_eq!(data_list(&primary, "AdditionalParts"), Some(Vec::new()));
    let alternate = f.row(alternate_path).await;
    assert_eq!(
        alternate.id,
        guid_to_db(f.id(BaseItemKind::Movie, alternate_path))
    );
    assert_eq!(kind_of(&alternate), "Movie");
    assert_eq!(alternate.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(alternate.primary_version_id.as_ref(), Some(&primary.id));
    let key = Uuid::parse_str(&primary.id).unwrap().simple().to_string();
    assert_eq!(alternate.presentation_unique_key, Some(key));
    assert_eq!(alternate.is_in_mixed_folder, primary.is_in_mixed_folder);
    assert_eq!(alternate.parent_id, primary.parent_id);
    assert_eq!(alternate.top_parent_id, primary.top_parent_id);
    assert_eq!(
        data_list(&alternate, "LocalAlternateVersions"),
        Some(Vec::new())
    );
    assert_eq!(f.links(&primary.id).await, vec![(alternate.id.clone(), 2)]);
}

/// Upstream `MovieResolverTests.Resolve_GivenLocalAlternateVersion_ResolvesToVideo`:
/// a local alternate version's file resolves to a video on its own — alone
/// in its folder it is the movie, and beside its primary it is a version
/// row of the primary's type.
#[tokio::test]
async fn a_local_alternate_version_resolves_to_a_video() {
    const VERSION: &str = "Black Panther (2018)/Black Panther (2018) - 1080p 3D.mk3d";
    let f = Fixture::movies(&[VERSION]).await;
    f.scanner.scan_all().await.unwrap();
    let alone = f.row(VERSION).await;
    assert_eq!(kind_of(&alone), "Movie");
    assert!(alone.owner_id.is_none());

    let f = Fixture::movies(&[VERSION, "Black Panther (2018)/Black Panther (2018).mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("Black Panther (2018)/Black Panther (2018).mkv").await;
    let version = f.row(VERSION).await;
    assert_eq!(kind_of(&version), "Movie");
    assert_eq!(version.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(
        data_list(&primary, "LocalAlternateVersions"),
        Some(vec![f.path(VERSION)])
    );
}

/// The group rules per library type (`ResolveMultipleInternal`,
/// `MovieResolver.cs:191-237`, and `FindMovie`'s `SupportsMultiVersion`,
/// `:472-476`): an own folder's versions group in a movies, music-video or
/// untyped library (a home-video library has no own folder), a plain
/// folder's only in a movies or music-video library; stacks everywhere.
#[rstest::rstest]
#[case::movies(Some(CollectionTypeOptions::movies), false, true)]
#[case::movies_plain(Some(CollectionTypeOptions::movies), true, true)]
#[case::music_videos(Some(CollectionTypeOptions::musicvideos), false, true)]
#[case::music_videos_plain(Some(CollectionTypeOptions::musicvideos), true, true)]
#[case::home_videos(Some(CollectionTypeOptions::homevideos), false, false)]
#[case::untyped(None, false, true)]
#[case::untyped_plain(None, true, false)]
#[case::mixed(Some(CollectionTypeOptions::mixed), false, true)]
#[tokio::test]
async fn versions_group_by_the_libraries_rules(
    #[case] kind: Option<CollectionTypeOptions>,
    #[case] plain: bool,
    #[case] grouped: bool,
) {
    let mut files = vec![
        "Film (2001)/Film (2001) - 1080p.mkv",
        "Film (2001)/Film (2001) - 720p.mkv",
        "Stack (2002)/Stack (2002) cd1.mkv",
        "Stack (2002)/Stack (2002) cd2.mkv",
    ];
    if plain {
        // A real subfolder: no folder is its movie's own.
        files.extend(["Film (2001)/Notes/a.txt", "Stack (2002)/Notes/a.txt"]);
    }
    let f = Fixture::new(&files, kind).await;
    f.scanner.scan_all().await.unwrap();

    let primary = f.row(files[0]).await;
    let other = f.row(files[1]).await;
    assert_eq!(primary.primary_version_id, None);
    if grouped {
        assert_eq!(other.owner_id.as_ref(), Some(&primary.id));
        assert_eq!(other.primary_version_id.as_ref(), Some(&primary.id));
        assert_eq!(other.type_, primary.type_, "the primary's type");
        assert_eq!(
            data_list(&primary, "LocalAlternateVersions"),
            Some(vec![f.path(files[1])])
        );
    } else {
        assert_eq!(other.owner_id, None);
        assert_eq!(
            data_list(&primary, "LocalAlternateVersions"),
            Some(Vec::new())
        );
        assert!(primary.is_in_mixed_folder && other.is_in_mixed_folder);
    }
    let stack = f.row(files[2]).await;
    let part = f.row(files[3]).await;
    assert_eq!(kind_of(&part), "Video");
    assert_eq!(part.owner_id.as_ref(), Some(&stack.id));
    assert_eq!(part.primary_version_id, None);
    assert_eq!(part.extra_type, None);
    assert_eq!(part.is_in_mixed_folder, stack.is_in_mixed_folder);
    assert_eq!(part.parent_id, stack.parent_id);
    assert_eq!(
        data_list(&stack, "AdditionalParts"),
        Some(vec![f.path(files[3])])
    );
}

/// An alternate version that is itself a stack: its own parts are its
/// `AdditionalParts` (`LibraryManager.SetAdditionalPartsFromStack`,
/// `LibraryManager.cs:828-862`) and `Video` rows it owns; the primary lists
/// only the alternate's first file.
#[tokio::test]
async fn an_alternate_versions_own_stack_is_its_parts() {
    let f = Fixture::movies(&[
        "Twins (1988)/Twins (1988).mkv",
        "Twins (1988)/Twins (1988) - 1080p part1.mkv",
        "Twins (1988)/Twins (1988) - 1080p part2.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("Twins (1988)/Twins (1988).mkv").await;
    let alternate = f.row("Twins (1988)/Twins (1988) - 1080p part1.mkv").await;
    let part = f.row("Twins (1988)/Twins (1988) - 1080p part2.mkv").await;
    assert_eq!(
        data_list(&primary, "LocalAlternateVersions"),
        Some(vec![f.path("Twins (1988)/Twins (1988) - 1080p part1.mkv")])
    );
    assert_eq!(alternate.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(
        data_list(&alternate, "AdditionalParts"),
        Some(vec![f.path("Twins (1988)/Twins (1988) - 1080p part2.mkv")])
    );
    assert_eq!(kind_of(&part), "Video");
    assert_eq!(part.owner_id.as_ref(), Some(&alternate.id));
    assert_eq!(f.links(&primary.id).await, vec![(alternate.id, 2)]);
}

/// Upstream `BaseItemTests.GetOwnerIdForExtra_AssignsExtraToItsVersion`
/// (`Video.GetOwnerIdForExtra`, `Video.cs:780-812`): an extra named after a
/// version is that version's, one named after the movie or in an extras
/// folder the primary's. And never a stacked part's: a part owns nothing.
#[rstest::rstest]
#[case("Movie/Movie - 4K-trailer.mkv", Some("Movie/Movie - 4K.mkv"))]
#[case(
    "Movie/Movie - 1080p-behindthescenes.mkv",
    Some("Movie/Movie - 1080p.mkv")
)]
#[case("Movie/Movie-trailer.mkv", None)]
#[case("Movie/trailers/Official.mkv", None)]
#[case("Movie/Movie - 4Kish-trailer.mkv", None)]
#[tokio::test]
async fn an_extra_is_owned_by_its_version(#[case] extra: &str, #[case] version: Option<&str>) {
    let f = Fixture::movies(&[
        "Movie/Movie.mkv",
        "Movie/Movie - 1080p.mkv",
        "Movie/Movie - 4K.mkv",
        extra,
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let owner = f.row(version.unwrap_or("Movie/Movie.mkv")).await;
    assert_eq!(f.row(extra).await.owner_id, Some(owner.id));
}

#[tokio::test]
async fn an_extra_named_after_a_stacked_part_is_the_primarys() {
    let f = Fixture::movies(&[
        "Movie/Movie cd1.mkv",
        "Movie/Movie cd2.mkv",
        "Movie/Movie cd2-trailer.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("Movie/Movie cd1.mkv").await;
    assert_eq!(
        f.row("Movie/Movie cd2-trailer.mkv").await.owner_id,
        Some(primary.id)
    );
}

/// Upstream `ResolveAlternateVersionTests.
/// ResolveAlternateVersion_StaleWrongTypeItem_DropsRowWithoutResavingPrimary`:
/// an alternate stored as a `Video` under the id that type derives, where
/// its primary is a `Movie`, is not kept beside the right-typed row. Upstream
/// deletes it (`DeleteItemsUnsafeFast`, `LibraryManager.cs:870-893`) and its
/// user data with it; Ferrofin moves it to the `Movie` id (owner decision
/// D9b), user data and all, and the primary keeps its row and id.
///
/// `…_DropsCachedParentListing` has no counterpart: Ferrofin keeps no cached
/// child listing on a folder.
#[tokio::test]
async fn a_wrong_type_alternate_moves_to_the_primarys_type() {
    const PRIMARY: &str = "Up/Up.mkv";
    const ALTERNATE: &str = "Up/Up - 1080p.mkv";
    let f = Fixture::movies(&[PRIMARY, ALTERNATE]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row(PRIMARY).await;
    let stale = guid_to_db(f.id(BaseItemKind::Video, ALTERNATE));
    f.restore_as(ALTERNATE, |row| {
        row.id.clone_from(&stale);
        row.type_ = stored_type_name(BaseItemKind::Video).unwrap().to_owned();
        row.name = Some("Up - 1080p".to_owned());
    })
    .await;
    let user = seed_user(&f.db).await;
    play(&f.db, &stale, user).await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(ALTERNATE).await, 1, "the stale row is gone");
    let alternate = f.row(ALTERNATE).await;
    assert_eq!(
        alternate.id,
        guid_to_db(f.id(BaseItemKind::Movie, ALTERNATE))
    );
    assert_eq!(kind_of(&alternate), "Movie");
    assert_eq!(alternate.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(alternate.primary_version_id.as_ref(), Some(&primary.id));
    assert!(f.played(&alternate.id, user).await, "its user data moved");
    assert_eq!(f.row(PRIMARY).await.id, primary.id);
}

/// The files of the adoption fixture's `Educational` home-video library
/// that resolve into stacks there, and their look-alikes.
const EDUCATIONAL: &[&str] = &[
    "Jack Joseph Puig Pt 1.mp4",
    "Jack Joseph Puig pt 2.mp4",
    "Jojo Mayer/Secret Weapons For The Modern Drummer - CD1.avi",
    "Jojo Mayer/Secret Weapons For The Modern Drummer - CD2.avi",
    "Jojo Mayer/Secret Weapons For The Modern Drummer - CD1.mp4",
    "Jojo Mayer/Secret Weapons For The Modern Drummer - CD2.mp4",
    "Jojo Mayer/Secret Weapons For The Modern Drummer - CD1 (1).avi",
    "Jojo Mayer/SWFMD 2 D1.mp4",
    "Jojo Mayer/SWFMD 2 D2.mp4",
    "Matt Garstka/Linear Drumming/Linear_Lesson_Part_1.mp4",
    "Matt Garstka/Linear Drumming/Linear_Lesson_part_2.mp4",
    "Matt Garstka/Single Pedal/Lesson_1_-_Bass_Drum_Placement_-_Part_1.mp4",
    "Matt Garstka/Single Pedal/Lesson_1_-_Bass_Drum_Placement_-_Part_2.mp4",
    "Matt Garstka/Single Pedal/Lesson_1_-_Bass_Drum_Placement_-_Part_3.mp4",
    "Matt Garstka/Universal Function/Universal_Function_-_Part_1.mp4",
    "Matt Garstka/Universal Function/Universal_Function_-_Part_2.mp4",
    "drumeo/Groove Essentials/groove-essentials-1-part-1.mp4",
    "drumeo/Groove Essentials/groove-essentials-1-part-2.mp4",
    "drumeo/Groove Essentials/groove-essentials-2-part-1.mp4",
    "drumeo/Groove Essentials/groove-essentials-2-part-2.mp4",
];

/// The stacks of a real Jellyfin 12.1 home-video library (the adoption
/// fixture's `Educational`, read from its database): raw names, a stack's
/// parts its `AdditionalParts`, the folder's mixed state from the number of
/// items `ResolveVideos` made — and files that only look like parts (a
/// different container, a ` (1)` copy) stand alone, as there.
#[tokio::test]
async fn home_video_stacks_match_jellyfin() {
    let f = Fixture::new(EDUCATIONAL, Some(CollectionTypeOptions::homevideos)).await;
    f.scanner.scan_all().await.unwrap();

    // (primary, its parts, its name and IsInMixedFolder in Jellyfin's row)
    let stacks: [(&str, &[&str], &str, bool); 7] = [
        (
            "Jack Joseph Puig Pt 1.mp4",
            &["Jack Joseph Puig pt 2.mp4"],
            "Jack Joseph Puig Pt 1",
            true,
        ),
        (
            "Jojo Mayer/Secret Weapons For The Modern Drummer - CD1.avi",
            &["Jojo Mayer/Secret Weapons For The Modern Drummer - CD2.avi"],
            "Secret Weapons For The Modern Drummer - CD1",
            true,
        ),
        (
            "Matt Garstka/Linear Drumming/Linear_Lesson_Part_1.mp4",
            &["Matt Garstka/Linear Drumming/Linear_Lesson_part_2.mp4"],
            "Linear_Lesson_Part_1",
            false,
        ),
        (
            "Matt Garstka/Single Pedal/Lesson_1_-_Bass_Drum_Placement_-_Part_1.mp4",
            &[
                "Matt Garstka/Single Pedal/Lesson_1_-_Bass_Drum_Placement_-_Part_2.mp4",
                "Matt Garstka/Single Pedal/Lesson_1_-_Bass_Drum_Placement_-_Part_3.mp4",
            ],
            "Lesson_1_-_Bass_Drum_Placement_-_Part_1",
            false,
        ),
        (
            "drumeo/Groove Essentials/groove-essentials-1-part-1.mp4",
            &["drumeo/Groove Essentials/groove-essentials-1-part-2.mp4"],
            "groove-essentials-1-part-1",
            true,
        ),
        (
            "drumeo/Groove Essentials/groove-essentials-2-part-1.mp4",
            &["drumeo/Groove Essentials/groove-essentials-2-part-2.mp4"],
            "groove-essentials-2-part-1",
            true,
        ),
        (
            "Matt Garstka/Universal Function/Universal_Function_-_Part_1.mp4",
            &["Matt Garstka/Universal Function/Universal_Function_-_Part_2.mp4"],
            "Universal_Function_-_Part_1",
            false,
        ),
    ];
    for (primary_path, parts, name, mixed) in stacks {
        let primary = f.row(primary_path).await;
        assert_eq!(kind_of(&primary), "Video", "{primary_path}");
        assert_eq!(primary.name.as_deref(), Some(name));
        assert_eq!(primary.is_in_mixed_folder, mixed, "{primary_path}");
        assert_eq!(primary.owner_id, None);
        assert_eq!(
            data_list(&primary, "AdditionalParts"),
            Some(parts.iter().map(|p| f.path(p)).collect::<Vec<_>>()),
            "{primary_path}"
        );
        for part_path in parts {
            let part = f.row(part_path).await;
            assert_eq!(kind_of(&part), "Video");
            assert_eq!(part.owner_id.as_ref(), Some(&primary.id), "{part_path}");
            assert_eq!(part.is_in_mixed_folder, mixed);
            let stem = part_path.rsplit('/').next().unwrap();
            let stem = stem.rsplit_once('.').unwrap().0;
            assert_eq!(part.name.as_deref(), Some(stem), "a part keeps its name");
        }
    }
    // Stood alone in Jellyfin's row too: no parts, no owner, mixed.
    for alone in [
        "Jojo Mayer/Secret Weapons For The Modern Drummer - CD1.mp4",
        "Jojo Mayer/Secret Weapons For The Modern Drummer - CD2.mp4",
        "Jojo Mayer/Secret Weapons For The Modern Drummer - CD1 (1).avi",
        "Jojo Mayer/SWFMD 2 D1.mp4",
        "Jojo Mayer/SWFMD 2 D2.mp4",
    ] {
        let row = f.row(alone).await;
        assert_eq!(row.owner_id, None, "{alone}");
        assert_eq!(data_list(&row, "AdditionalParts"), Some(Vec::new()));
        assert!(row.is_in_mixed_folder, "{alone}");
    }
}

/// The bench's adopted alternates (40 movies there): Jellyfin 10.11 stored
/// `Movie (Year) - 1080p.mkv` beside its `- 2160p` primary as a `Video`
/// under its own id — owned by the primary, pointing at nothing, its own
/// presentation key, no parent, not in a mixed folder, raw name — and the
/// primary listing it. The scan moves it to the `Movie` id (D9b) as the
/// primary's local version: owned, pointing at it, under its key and parent,
/// linked; its stored name stands. The next scan writes nothing.
#[tokio::test]
async fn an_adopted_alternate_becomes_the_primarys_local_version() {
    const PRIMARY: &str = "Winter of Factory (1978)/Winter of Factory (1978) - 2160p.mkv";
    const ALTERNATE: &str = "Winter of Factory (1978)/Winter of Factory (1978) - 1080p.mkv";
    let f = Fixture::movies(&[PRIMARY, ALTERNATE]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row(PRIMARY).await;
    let adopted = guid_to_db(f.id(BaseItemKind::Video, ALTERNATE));
    sqlx::query(r#"DELETE FROM "LinkedChildren""#)
        .execute(f.db.writer())
        .await
        .unwrap();
    f.restore_as(ALTERNATE, |row| {
        row.id.clone_from(&adopted);
        row.type_ = stored_type_name(BaseItemKind::Video).unwrap().to_owned();
        row.name = Some("Winter of Factory (1978) - 1080p".to_owned());
        row.primary_version_id = None;
        row.presentation_unique_key = None;
        row.parent_id = None;
        row.top_parent_id = None;
        row.is_in_mixed_folder = false;
    })
    .await;
    let user = seed_user(&f.db).await;
    play(&f.db, &adopted, user).await;

    f.scanner.scan_all().await.unwrap();
    let alternate = f.row(ALTERNATE).await;
    assert_eq!(
        alternate.id,
        guid_to_db(f.id(BaseItemKind::Movie, ALTERNATE))
    );
    assert_eq!(kind_of(&alternate), "Movie");
    assert_eq!(alternate.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(alternate.primary_version_id.as_ref(), Some(&primary.id));
    let key = Uuid::parse_str(&primary.id).unwrap().simple().to_string();
    assert_eq!(alternate.presentation_unique_key, Some(key));
    assert_eq!(alternate.parent_id, primary.parent_id);
    assert_eq!(
        alternate.name.as_deref(),
        Some("Winter of Factory (1978) - 1080p")
    );
    assert!(f.played(&alternate.id, user).await);
    assert_eq!(f.links(&primary.id).await, vec![(alternate.id, 2)]);

    count_item_writes(&f.db).await;
    let quiet = f.scanner.scan_all().await.unwrap();
    assert_eq!((quiet.created, quiet.updated), (0, 0));
    assert_eq!(item_writes(&f.db).await, 0);
}

/// The bench's adopted Jellyfin 10.11 groups, repaired at boot
/// (`rehome_local_versions`, owner decision D9c): a local version stored as
/// a `Video` its primary owns — no parent, top parent, pointer, ancestors
/// or link, keyed by its own id, played — and a stacked part owned with no
/// parent or ancestors. After the repair the first scan writes no
/// `BaseItems` row and the user data is where it was put.
#[tokio::test]
async fn an_adopted_group_repaired_at_boot_scans_quietly() {
    const PRIMARY: &str = "Bridge Forest (2014)/Bridge Forest (2014) - 2160p.mkv";
    const ALTERNATE: &str = "Bridge Forest (2014)/Bridge Forest (2014) - 1080p.mkv";
    const PART: &str = "Heat (1995)/Heat (1995) cd2.mkv";
    let f = Fixture::movies(&[PRIMARY, ALTERNATE, "Heat (1995)/Heat (1995) cd1.mkv", PART]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row(PRIMARY).await;
    let planned = f.row(ALTERNATE).await;
    let part = f.row(PART).await;
    sqlx::query(r#"DELETE FROM "LinkedChildren""#)
        .execute(f.db.writer())
        .await
        .unwrap();
    let adopted = guid_to_db(f.id(BaseItemKind::Video, ALTERNATE));
    f.restore_as(ALTERNATE, |row| {
        row.id.clone_from(&adopted);
        row.type_ = stored_type_name(BaseItemKind::Video).unwrap().to_owned();
        // 10.11's own name for it, and the sort key the save derives from it.
        row.name = Some("Bridge Forest (2014) - 1080p".to_owned());
        row.sort_name = None;
        row.primary_version_id = None;
        row.presentation_unique_key =
            Some(Uuid::parse_str(&adopted).unwrap().as_simple().to_string());
        row.parent_id = None;
        row.top_parent_id = None;
    })
    .await;
    f.restore_as(PART, |row| {
        row.parent_id = None;
        row.top_parent_id = None;
        row.is_in_mixed_folder = true;
    })
    .await;
    let user = seed_user(&f.db).await;
    play(&f.db, &adopted, user).await;

    let rehomed = ferrofin_core::adoption_repairs::rehome_local_versions(
        &f.db,
        &IdDerivation::LegacyLowercase,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        (rehomed.rekeyed, rehomed.versions, rehomed.parts),
        (1, 1, 1)
    );
    let alternate = f.row(ALTERNATE).await;
    assert_eq!(alternate.id, planned.id);
    assert_eq!(
        alternate.name.as_deref(),
        Some("Bridge Forest (2014) - 1080p")
    );
    assert!(f.played(&alternate.id, user).await);
    assert_eq!(f.links(&primary.id).await, vec![(alternate.id.clone(), 2)]);
    assert_eq!(f.row(PART).await.parent_id, part.parent_id);

    count_item_writes(&f.db).await;
    let first = f.scanner.scan_all().await.unwrap();
    assert_eq!((first.created, first.updated), (0, 0), "{first:?}");
    assert_eq!(item_writes(&f.db).await, 0);
    assert!(f.played(&alternate.id, user).await, "its user data stays");
    let part_now = f.row(PART).await;
    assert_eq!(part_now.id, part.id);
    assert!(
        !part_now.is_in_mixed_folder,
        "the part took its primary's folder state"
    );
}

/// An older Ferrofin scan stored a stack's second part as a `Movie` of its
/// own: the scan moves it to the `Video` id a part derives, owned by the
/// primary, with its user data (S2's re-key). When the stack breaks — the
/// primary's file goes — the part is a movie again and moves back, its user
/// data still with it, while the old primary is pruned.
#[tokio::test]
async fn a_part_moves_to_its_kind_and_back_with_its_user_data() {
    const FIRST: &str = "Heat (1995)/Heat (1995) cd1.mkv";
    const SECOND: &str = "Heat (1995)/Heat (1995) cd2.mkv";
    let f = Fixture::movies(&[FIRST, SECOND]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row(FIRST).await;
    let older = guid_to_db(f.id(BaseItemKind::Movie, SECOND));
    f.restore_as(SECOND, |row| {
        row.id.clone_from(&older);
        row.type_ = stored_type_name(BaseItemKind::Movie).unwrap().to_owned();
        row.owner_id = None;
    })
    .await;
    let user = seed_user(&f.db).await;
    play(&f.db, &older, user).await;

    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(SECOND).await, 1);
    let part = f.row(SECOND).await;
    assert_eq!(part.id, guid_to_db(f.id(BaseItemKind::Video, SECOND)));
    assert_eq!(part.owner_id.as_ref(), Some(&primary.id));
    assert!(f.played(&part.id, user).await);

    std::fs::remove_file(f.path(FIRST)).unwrap();
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.rows_at(FIRST).await, 0, "the old primary is pruned");
    let movie = f.row(SECOND).await;
    assert_eq!(movie.id, older);
    assert_eq!(kind_of(&movie), "Movie");
    assert_eq!(movie.owner_id, None);
    assert!(f.played(&movie.id, user).await);
}

/// An older Ferrofin scan stored a local alternate as a movie of its own
/// (same kind, same id, no owner, no link): the scan keeps its id and makes
/// it the primary's version — owner, pointer, key and link.
#[tokio::test]
async fn an_unowned_alternate_keeps_its_id_and_joins_its_primary() {
    const PRIMARY: &str = "Ronin (1998)/Ronin (1998) - 2160p.mkv";
    const ALTERNATE: &str = "Ronin (1998)/Ronin (1998) - 1080p.mkv";
    let f = Fixture::movies(&[PRIMARY, ALTERNATE]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row(PRIMARY).await;
    sqlx::query(r#"DELETE FROM "LinkedChildren""#)
        .execute(f.db.writer())
        .await
        .unwrap();
    let id = f
        .restore_as(ALTERNATE, |row| {
            row.owner_id = None;
            row.primary_version_id = None;
            row.presentation_unique_key = None;
        })
        .await;

    f.scanner.scan_all().await.unwrap();
    let alternate = f.row(ALTERNATE).await;
    assert_eq!(alternate.id, id);
    assert_eq!(alternate.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(alternate.primary_version_id.as_ref(), Some(&primary.id));
    let key = Uuid::parse_str(&primary.id).unwrap().simple().to_string();
    assert_eq!(alternate.presentation_unique_key, Some(key));
    assert_eq!(f.links(&primary.id).await, vec![(id, 2)]);
}

/// A stored part keeps the name and the mixed-folder state it has: its
/// primary sets them only when it creates it (`Video.cs:686-691`).
#[tokio::test]
async fn a_stored_part_keeps_its_name_and_mixed_folder_state() {
    const SECOND: &str = "Heat (1995)/Heat (1995) cd2.mkv";
    let f = Fixture::movies(&["Heat (1995)/Heat (1995) cd1.mkv", SECOND]).await;
    f.scanner.scan_all().await.unwrap();
    let part = f.row(SECOND).await;
    assert!(!part.is_in_mixed_folder);
    sqlx::query(
        r#"UPDATE "BaseItems" SET "Name" = 'Heat, part two', "IsInMixedFolder" = 1
           WHERE "Id" = ?1"#,
    )
    .bind(&part.id)
    .execute(f.db.writer())
    .await
    .unwrap();

    f.scanner.scan_all().await.unwrap();
    let part = f.row(SECOND).await;
    assert_eq!(part.name.as_deref(), Some("Heat, part two"));
    assert!(part.is_in_mixed_folder);
}

/// A library of stacks and version groups — an alternate with its own
/// stack among them — rescans quietly: nothing created or updated, no
/// `BaseItems` row written. So does an untyped library's own-folder movie
/// and a music video's, whose alternates the planner dates as their first
/// refresh would (`Movie`/`MusicVideo.BeforeMetadataRefresh`: the year in
/// the name, else the unmixed folder's).
#[rstest::rstest]
#[case::movies(Some(CollectionTypeOptions::movies), &[
    "Heat (1995)/Heat (1995) cd1.mkv",
    "Heat (1995)/Heat (1995) cd2.mkv",
    "Ronin (1998)/Ronin (1998) - 2160p.mkv",
    "Ronin (1998)/Ronin (1998) - 1080p.mkv",
    "Twins (1988)/Twins (1988).mkv",
    "Twins (1988)/Twins (1988) - 1080p part1.mkv",
    "Twins (1988)/Twins (1988) - 1080p part2.mkv",
    "Heist cd1.mkv",
    "Heist cd2.mkv",
])]
#[case::untyped(None, &["Film (2001)/Film (2001).mkv", "Film (2001)/Film (2001) - 720p.mkv"])]
#[case::music_videos(Some(CollectionTypeOptions::musicvideos), &[
    "Movie (2001)/Movie (2001).mkv",
    "Movie (2001)/Movie (2001) - 1080p.mkv",
])]
// Not named after their folder: no group, two videos of a mixed folder.
#[case::music_videos_apart(Some(CollectionTypeOptions::musicvideos), &[
    "Movie (2001)/Movie.mkv",
    "Movie (2001)/Movie - 1080p.mkv",
])]
#[tokio::test]
async fn a_grouped_library_rescans_quietly(
    #[case] kind: Option<CollectionTypeOptions>,
    #[case] files: &[&str],
) {
    let f = Fixture::new(files, kind).await;
    f.scanner.scan_all().await.unwrap();
    let alternate = f.row(files[1]).await;
    if kind != Some(CollectionTypeOptions::movies) && files[1].contains("(2001) -") {
        assert!(alternate.owner_id.is_some(), "grouped");
        assert_eq!(alternate.production_year, Some(2001));
    }
    count_item_writes(&f.db).await;
    let second = f.scanner.scan_all().await.unwrap();
    assert_eq!((second.created, second.updated), (0, 0), "{second:?}");
    assert_eq!(item_writes(&f.db).await, 0);
}

/// A path-scoped scan of a new alternate version refreshes its whole group:
/// the alternate is created as the primary's version and the primary lists
/// it, though the scan named only the new file.
#[tokio::test]
async fn a_scoped_scan_of_a_new_version_refreshes_its_group() {
    const PRIMARY: &str = "Film/Film.mkv";
    const VERSION: &str = "Film/Film - 1080p.mkv";
    let f = Fixture::movies(&[PRIMARY]).await;
    f.scanner.scan_all().await.unwrap();
    touch(&f.media, VERSION);
    f.scanner.scan_paths(&[f.path(VERSION)]).await.unwrap();

    let primary = f.row(PRIMARY).await;
    let version = f.row(VERSION).await;
    assert_eq!(version.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(
        data_list(&primary, "LocalAlternateVersions"),
        Some(vec![f.path(VERSION)])
    );
    assert_eq!(f.links(&primary.id).await, vec![(version.id, 2)]);
}

/// A library location's loose files each root a path-scoped scan at
/// themselves, so a stack's primary deleted there would leave its second
/// part unplanned while the prune of the primary — which follows `OwnerId` —
/// took it and its user data along. The scan widens a removed file to its
/// stored group: the part is planned, a movie again, its user data kept.
#[tokio::test]
async fn a_scoped_scan_of_a_removed_root_primary_keeps_its_part() {
    let f = Fixture::movies(&["Heist cd1.mkv", "Heist cd2.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let part = f.row("Heist cd2.mkv").await;
    assert!(part.owner_id.is_some());
    let user = seed_user(&f.db).await;
    play(&f.db, &part.id, user).await;

    std::fs::remove_file(f.path("Heist cd1.mkv")).unwrap();
    f.scanner
        .scan_paths(&[f.path("Heist cd1.mkv")])
        .await
        .unwrap();
    assert_eq!(f.rows_at("Heist cd1.mkv").await, 0, "the primary is pruned");
    let movie = f.row("Heist cd2.mkv").await;
    assert_eq!(kind_of(&movie), "Movie");
    assert_eq!(movie.owner_id, None);
    assert!(f.played(&movie.id, user).await, "its user data is kept");
}

/// A root stack's primary renamed out of the stack: the scan of the old
/// and the new path plans the new movie and the part left behind, which
/// stands alone now, its user data kept.
#[tokio::test]
async fn a_scoped_scan_of_a_renamed_root_primary_keeps_its_part() {
    let f = Fixture::movies(&["Heist cd1.mkv", "Heist cd2.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let part = f.row("Heist cd2.mkv").await;
    let user = seed_user(&f.db).await;
    play(&f.db, &part.id, user).await;

    std::fs::rename(f.path("Heist cd1.mkv"), f.path("Caper.mkv")).unwrap();
    f.scanner
        .scan_paths(&[f.path("Heist cd1.mkv"), f.path("Caper.mkv")])
        .await
        .unwrap();
    assert_eq!(f.rows_at("Heist cd1.mkv").await, 0);
    assert_eq!(kind_of(&f.row("Caper.mkv").await), "Movie");
    let movie = f.row("Heist cd2.mkv").await;
    assert_eq!(movie.owner_id, None);
    assert!(f.played(&movie.id, user).await);
}

/// Belt and braces: a scan that does not plan a part or version whose owner
/// it prunes — a folder's scan, the owned row in another folder — releases
/// it (no owner, no version pointer, no local-version link) rather than
/// letting the owner's delete take it and its user data; the next full scan
/// plans it on its own.
#[tokio::test]
async fn the_prune_releases_a_surviving_part_it_did_not_plan() {
    let f = Fixture::movies(&["A/Film cd1.mkv", "A/Film cd2.mkv", "B/Other.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("A/Film cd1.mkv").await;
    // The owned row is elsewhere: no walk of `A` plans it.
    let other = f.row("B/Other.mkv").await;
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
    play(&f.db, &other.id, user).await;

    std::fs::remove_file(f.path("A/Film cd1.mkv")).unwrap();
    f.scanner.scan_paths(&[f.path("A")]).await.unwrap();
    assert_eq!(
        f.rows_at("A/Film cd1.mkv").await,
        0,
        "the primary is pruned"
    );
    let released = f.row("B/Other.mkv").await;
    assert_eq!(released.id, other.id);
    assert_eq!(
        (released.owner_id, released.primary_version_id),
        (None, None)
    );
    let key = Uuid::parse_str(&other.id).unwrap().simple().to_string();
    assert_eq!(released.presentation_unique_key, Some(key));
    assert!(f.played(&other.id, user).await);
}

/// A library-root stack gains its second part: the path-scoped scan roots
/// at the new file alone, plans the stack whole and saves the primary it
/// belongs to — with its new `AdditionalParts` — without refreshing it.
/// And a new primary refreshes the part it now owns along with it.
#[tokio::test]
async fn a_scoped_scan_of_a_new_root_part_regroups_its_stack() {
    let f = Fixture::movies(&["Heist cd1.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    touch(&f.media, "Heist cd2.mkv");
    f.scanner
        .scan_paths(&[f.path("Heist cd2.mkv")])
        .await
        .unwrap();
    let primary = f.row("Heist cd1.mkv").await;
    let part = f.row("Heist cd2.mkv").await;
    assert_eq!(part.owner_id.as_ref(), Some(&primary.id));
    assert_eq!(
        data_list(&primary, "AdditionalParts"),
        Some(vec![f.path("Heist cd2.mkv")])
    );

    let f = Fixture::movies(&["Caper cd2.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    touch(&f.media, "Caper cd1.mkv");
    f.scanner
        .scan_paths(&[f.path("Caper cd1.mkv")])
        .await
        .unwrap();
    let primary = f.row("Caper cd1.mkv").await;
    let part = f.row("Caper cd2.mkv").await;
    assert_eq!(kind_of(&part), "Video");
    assert_eq!(part.owner_id.as_ref(), Some(&primary.id));
    assert!(part.date_last_refreshed.is_some());
}

/// A refresh of one local version replacing all its metadata leaves the
/// version's primary as it was: a version's refresh never reaches the item
/// it belongs to, which the scan only saves with what the walk resolved.
#[tokio::test]
async fn replacing_one_versions_metadata_leaves_its_primarys() {
    use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
    const PRIMARY: &str = "Ronin (1998)/Ronin (1998) - 2160p.mkv";
    const VERSION: &str = "Ronin (1998)/Ronin (1998) - 1080p.mkv";
    let f = Fixture::movies(&[PRIMARY, VERSION]).await;
    std::fs::write(
        f.path("Ronin (1998)/movie.nfo"),
        "<movie><title>Ronin</title><plot>From the NFO.</plot></movie>",
    )
    .unwrap();
    f.scanner.scan_all().await.unwrap();
    sqlx::query(r#"UPDATE "BaseItems" SET "Overview" = 'Edited' WHERE "Path" IN (?1, ?2)"#)
        .bind(f.path(PRIMARY))
        .bind(f.path(VERSION))
        .execute(f.db.writer())
        .await
        .unwrap();
    let replace_all = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        ..MetadataRefreshOptions::default()
    };
    f.scanner
        .scan_paths_with(&[f.path(VERSION)], &replace_all)
        .await
        .unwrap();
    assert_eq!(
        f.row(VERSION).await.overview.as_deref(),
        Some("From the NFO."),
        "the version is replaced"
    );
    assert_eq!(f.row(PRIMARY).await.overview.as_deref(), Some("Edited"));

    // The same refresh of the primary does reach it.
    f.scanner
        .scan_paths_with(&[f.path(PRIMARY)], &replace_all)
        .await
        .unwrap();
    assert_eq!(
        f.row(PRIMARY).await.overview.as_deref(),
        Some("From the NFO.")
    );
}

/// An alternate version whose first file is a `.disc` stub — which the walk
/// does not plan — leaves its other file to stand on its own rather than
/// unplanned.
#[tokio::test]
async fn a_stub_headed_alternates_other_file_stands_alone() {
    let f = Fixture::movies(&[
        "Movie/Movie.mkv",
        "Movie/Movie - 1080p cd1.disc",
        "Movie/Movie - 1080p cd2.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("Movie/Movie.mkv").await;
    assert_eq!(
        data_list(&primary, "LocalAlternateVersions"),
        Some(Vec::new())
    );
    let other = f.row("Movie/Movie - 1080p cd2.mkv").await;
    assert_eq!(kind_of(&other), "Movie");
    assert_eq!(other.owner_id, None);
    assert_eq!(f.rows_at("Movie/Movie - 1080p cd1.disc").await, 0);
}

/// Step 12 S6. A saved primary gives its local versions its metadata
/// (`Video.UpdateToRepositoryAsync`, `Video.cs:704-726`: overview, year,
/// premiere date, community and official rating, genres, provider ids,
/// images) and its stacked parts its title metadata (`copyTitleMetadata`,
/// `BaseItem.cs:2805-2858`: also studios, production locations, critic and
/// custom rating, and the dates it has) — whichever row the walk saved
/// first, the primary's values stand. The genres reach the versions'
/// `ItemValues` links, and a rescan writes nothing.
#[tokio::test]
async fn a_primarys_metadata_reaches_its_versions_and_parts_once() {
    const PRIMARY: &str = "Heat (1995)/Heat (1995).mkv";
    const VERSION: &str = "Heat (1995)/Heat (1995) - 1080p.mkv";
    const FIRST: &str = "Ronin (1998)/Ronin (1998) cd1.mkv";
    const PART: &str = "Ronin (1998)/Ronin (1998) cd2.mkv";
    let f = Fixture::movies(&[PRIMARY, VERSION, FIRST, PART]).await;
    let nfo = |title: &str, year: i32| {
        format!(
            "<movie><title>{title}</title><plot>A heist.</plot><genre>Crime</genre>\
             <genre>Drama</genre><studio>Warner</studio><country>USA</country>\
             <rating>8.3</rating><criticrating>87</criticrating><mpaa>R</mpaa>\
             <customrating>Adults</customrating><year>{year}</year>\
             <premiered>{year}-12-15</premiered><uniqueid type=\"imdb\">tt0113277</uniqueid></movie>"
        )
    };
    std::fs::write(f.path("Heat (1995)/Heat (1995).nfo"), nfo("Heat", 1995)).unwrap();
    std::fs::write(
        f.path("Ronin (1998)/Ronin (1998) cd1.nfo"),
        nfo("Ronin", 1998),
    )
    .unwrap();
    f.scanner.scan_all().await.unwrap();

    let primary = f.row(PRIMARY).await;
    let version = f.row(VERSION).await;
    assert_eq!(version.primary_version_id.as_ref(), Some(&primary.id));
    assert_eq!(primary.overview.as_deref(), Some("A heist."));
    for (field, theirs, mine) in [
        ("Overview", &primary.overview, &version.overview),
        ("Genres", &primary.genres, &version.genres),
        (
            "OfficialRating",
            &primary.official_rating,
            &version.official_rating,
        ),
    ] {
        assert_eq!(mine, theirs, "{field}");
    }
    assert_eq!(version.production_year, Some(1995));
    assert_eq!(version.premiere_date, primary.premiere_date);
    assert_eq!(version.community_rating, primary.community_rating);
    let ids = |id: String| {
        let db = f.db.clone();
        async move {
            sqlx::query_as::<_, (String, String)>(
                r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ItemId" = ? ORDER BY 1"#,
            )
            .bind(id)
            .fetch_all(db.pool())
            .await
            .unwrap()
        }
    };
    assert_eq!(ids(version.id.clone()).await, ids(primary.id.clone()).await);
    assert!(!ids(primary.id.clone()).await.is_empty());
    let genre_links: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "ItemValuesMap" m JOIN "ItemValues" v
           ON v."ItemValueId" = m."ItemValueId" WHERE m."ItemId" = ? AND v."Type" = 2"#,
    )
    .bind(&version.id)
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(genre_links, 2, "the version's genres are indexed");

    let first = f.row(FIRST).await;
    let part = f.row(PART).await;
    assert_eq!(part.owner_id.as_ref(), Some(&first.id));
    assert_eq!(part.overview.as_deref(), Some("A heist."));
    assert_eq!(part.genres, first.genres);
    assert_eq!(part.studios, first.studios);
    assert_eq!(part.production_locations, first.production_locations);
    assert_eq!(part.community_rating, first.community_rating);
    assert_eq!(part.critic_rating, first.critic_rating);
    assert_eq!(part.official_rating.as_deref(), Some("R"));
    assert_eq!(part.custom_rating, first.custom_rating);
    assert_eq!(part.production_year, Some(1998));
    assert_eq!(part.premiere_date, first.premiere_date);

    // This fixture has no media prober, so every NFO-backed movie lacks its
    // media info and refreshes — and is saved — on every scan: a primary
    // saved with the metadata its owned rows already carry copies nothing.
    let owned = || async { (f.row(VERSION).await, f.row(PART).await) };
    let before = owned().await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(owned().await, before, "the owned rows are not rewritten");

    // With its media info in place nothing refreshes, and nothing is written.
    sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = 1 WHERE "IsFolder" = 0"#)
        .execute(f.db.writer())
        .await
        .unwrap();
    count_item_writes(&f.db).await;
    let second = f.scanner.scan_all().await.unwrap();
    assert_eq!((second.created, second.updated), (0, 0), "{second:?}");
    assert_eq!(item_writes(&f.db).await, 0);
}

/// `GET /Videos/{id}/AdditionalParts`' read (`Video.GetAdditionalParts`):
/// a stack's primary lists its other parts by `SortName`; a part, a lone
/// movie and a version group list none.
#[tokio::test]
async fn a_stacks_primary_lists_its_additional_parts() {
    use ferrofin_traits::library::LibraryManager as _;
    let f = Fixture::movies(&[
        "Heist cd1.mkv",
        "Heist cd3.mkv",
        "Heist cd2.mkv",
        "Ronin (1998)/Ronin (1998).mkv",
        "Ronin (1998)/Ronin (1998) - 1080p.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let repo = Arc::new(FerrofinItemRepository::new(
        f.db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let manager = ferrofin_core::FerrofinLibraryManager::new(
        repo,
        Arc::new(ferrofin_core::FerrofinItemCountService::new(f.db.clone())),
        f.store.clone(),
        Arc::new(ferrofin_core::FerrofinPeopleRepository::new(f.db.clone())),
    )
    .with_id_derivation(IdDerivation::LegacyLowercase);
    let parts = manager
        .get_additional_parts(&f.row("Heist cd1.mkv").await, None)
        .await
        .unwrap();
    assert_eq!(
        parts
            .iter()
            .map(|p| p.path.clone().unwrap())
            .collect::<Vec<_>>(),
        vec![f.path("Heist cd2.mkv"), f.path("Heist cd3.mkv")]
    );
    for none in ["Heist cd2.mkv", "Ronin (1998)/Ronin (1998).mkv"] {
        assert!(
            manager
                .get_additional_parts(&f.row(none).await, None)
                .await
                .unwrap()
                .is_empty(),
            "{none}"
        );
    }
}

/// The parts of a stack take its ratings, and so its parental score
/// (`BaseItem.OnMetadataChanged`, run by every save): a user held below an
/// R rating is not handed the parts of an R movie by
/// `GET /Videos/{id}/AdditionalParts` (`Video.GetAdditionalParts(user)`).
#[tokio::test]
async fn a_restricted_user_gets_no_parts_of_a_movie_rated_above_them() {
    use ferrofin_traits::library::LibraryManager as _;
    let f = Fixture::movies(&[
        "Heist (2001)/Heist (2001) cd1.mkv",
        "Heist (2001)/Heist (2001) cd2.mkv",
    ])
    .await;
    std::fs::write(
        f.path("Heist (2001)/Heist (2001) cd1.nfo"),
        "<movie><title>Heist</title><mpaa>R</mpaa></movie>",
    )
    .unwrap();
    let localization = Arc::new(ferrofin_core::LocalizationManager::new("US"));
    f.store.set_localization(localization.clone());
    let f = Fixture {
        scanner: f.scanner.with_localization(localization),
        ..f
    };
    f.scanner.scan_all().await.unwrap();
    let part = f.row("Heist (2001)/Heist (2001) cd2.mkv").await;
    assert_eq!(part.official_rating.as_deref(), Some("R"));
    assert_eq!(
        part.inherited_parental_rating_value,
        f.row("Heist (2001)/Heist (2001) cd1.mkv")
            .await
            .inherited_parental_rating_value,
        "the part is scored as its owner"
    );
    assert!(part.inherited_parental_rating_value.is_some());

    let manager = ferrofin_core::FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(
            f.db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        Arc::new(ferrofin_core::FerrofinItemCountService::new(f.db.clone())),
        f.store.clone(),
        Arc::new(ferrofin_core::FerrofinPeopleRepository::new(f.db.clone())),
    )
    .with_id_derivation(IdDerivation::LegacyLowercase);
    let user = seed_user(&f.db).await;
    let user_row = |max: Option<i64>| {
        let db = f.db.clone();
        async move {
            sqlx::query(r#"UPDATE "Users" SET "MaxParentalRatingScore" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(user))
                .bind(max)
                .execute(db.writer())
                .await
                .unwrap();
            sqlx::query_as::<_, ferrofin_db::entities::users::UserEntity>(
                r#"SELECT * FROM "Users" WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(user))
            .fetch_one(db.pool())
            .await
            .unwrap()
        }
    };
    let primary = f.row("Heist (2001)/Heist (2001) cd1.mkv").await;
    let unrestricted = user_row(None).await;
    assert_eq!(
        manager
            .get_additional_parts(&primary, Some(&unrestricted))
            .await
            .unwrap()
            .len(),
        1
    );
    let restricted = user_row(Some(13)).await;
    assert!(
        manager
            .get_additional_parts(&primary, Some(&restricted))
            .await
            .unwrap()
            .is_empty()
    );
}

/// A local version with an NFO of its own keeps what its NFO says until
/// its primary is saved, then takes the primary's — and rescans of the
/// group stay quiet rather than flip between the two.
#[tokio::test]
async fn a_version_with_its_own_nfo_does_not_flip_flop() {
    const PRIMARY: &str = "Heat (1995)/Heat (1995).mkv";
    const VERSION: &str = "Heat (1995)/Heat (1995) - 1080p.mkv";
    let f = Fixture::movies(&[PRIMARY, VERSION]).await;
    std::fs::write(
        f.path("Heat (1995)/Heat (1995).nfo"),
        "<movie><title>Heat</title><plot>The primary's.</plot></movie>",
    )
    .unwrap();
    std::fs::write(
        f.path("Heat (1995)/Heat (1995) - 1080p.nfo"),
        "<movie><title>Heat</title><plot>The version's own.</plot></movie>",
    )
    .unwrap();
    f.scanner.scan_all().await.unwrap();
    assert_eq!(
        f.row(VERSION).await.overview.as_deref(),
        Some("The primary's."),
        "the primary's save wins"
    );
    // Media info in place, so nothing refreshes for want of it.
    sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = 1 WHERE "IsFolder" = 0"#)
        .execute(f.db.writer())
        .await
        .unwrap();
    count_item_writes(&f.db).await;
    for _ in 0..2 {
        let rescan = f.scanner.scan_all().await.unwrap();
        assert_eq!((rescan.created, rescan.updated), (0, 0), "{rescan:?}");
    }
    assert_eq!(item_writes(&f.db).await, 0);
    assert_eq!(
        f.row(VERSION).await.overview.as_deref(),
        Some("The primary's.")
    );
}

/// The scan scores an item's custom rating ahead of its official one
/// (`GetParentalRatingScore`): a row stored with the official rating's score
/// (as the scan once stamped it) is rewritten once, and the next scan is
/// quiet; a rating removed from the item clears its stored score
/// (`OnMetadataChanged`'s else branch).
#[tokio::test]
async fn the_scan_scores_the_custom_rating_first_and_clears_a_lost_one() {
    const MOVIE: &str = "Heat (1995)/Heat (1995).mkv";
    const SOLO: &str = "Solo (2000)/Solo (2000).mkv";
    let f = Fixture::movies(&[MOVIE, SOLO]).await;
    std::fs::write(
        f.path("Heat (1995)/Heat (1995).nfo"),
        "<movie><title>Heat</title><mpaa>R</mpaa><customrating>PG</customrating></movie>",
    )
    .unwrap();
    let localization = Arc::new(ferrofin_core::LocalizationManager::new("US"));
    let score = |rating: &str| {
        localization
            .get_rating_score(rating, None)
            .map(|s| i64::from(s.score))
    };
    let (pg, r) = (score("PG"), score("R"));
    assert_ne!(pg, r);
    let f = Fixture {
        scanner: f.scanner.with_localization(localization.clone()),
        ..f
    };
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(MOVIE).await.inherited_parental_rating_value, pg);

    // The official rating's score, as stored before; and a stale score on a
    // movie with no rating at all. Media info in place, so nothing else
    // refreshes.
    sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = 1 WHERE "IsFolder" = 0"#)
        .execute(f.db.writer())
        .await
        .unwrap();
    for (path, value) in [(MOVIE, r), (SOLO, Some(13))] {
        sqlx::query(
            r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = ?2 WHERE "Path" = ?1"#,
        )
        .bind(f.path(path))
        .bind(value)
        .execute(f.db.writer())
        .await
        .unwrap();
    }
    let once = f.scanner.scan_all().await.unwrap();
    assert_eq!(once.updated, 2, "{once:?}");
    assert_eq!(f.row(MOVIE).await.inherited_parental_rating_value, pg);
    assert_eq!(f.row(SOLO).await.inherited_parental_rating_value, None);

    count_item_writes(&f.db).await;
    let again = f.scanner.scan_all().await.unwrap();
    assert_eq!((again.created, again.updated), (0, 0), "{again:?}");
    assert_eq!(item_writes(&f.db).await, 0);
}
