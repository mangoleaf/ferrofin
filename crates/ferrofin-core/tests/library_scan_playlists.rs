//! Playlists a library scan resolves off disk (`PlaylistResolver`), end to
//! end through a real scan over the real repositories and SQLite schema:
//!
//! - a playlist FILE in a music library is a `Playlist` under the folder
//!   item it sits in, named after the file, its members the file's entries
//!   in order (repeats kept, entries naming no item dropped), whatever form
//!   they take — relative, absolute, `../`;
//! - a `.pls` is that playlist, never a track — a track an older scan made
//!   of one is pruned;
//! - a `[playlist]` folder is a playlist, and nothing in it is an item;
//! - an unchanged rescan leaves the members alone, an edited file replaces
//!   them, a full refresh that keeps metadata puts the file's first and
//!   keeps the rest, and a deleted file takes its playlist with it.
//!
//! The rows and members match what a real Jellyfin 12.2 stores for the same
//! tree when it writes no XML sidecar beside the playlist files (see
//! `library_scan/playlists.rs` for the case where it does) — but for
//! `Lists/top.m3u8`'s parent: Jellyfin makes `Lists`, neither artist nor
//! album, a plain `Folder`, and a music scan here plans no such row, so the
//! playlist hangs off the library.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

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
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

/// A music library over the oracle's tree:
///
/// ```text
/// music/Test Artist/First Album/01 - One.mp3, 02 - Two.mp3, 03 - Three.mp3
/// music/Test Artist/First Album/Album.m3u      → Three, One, Three, missing
/// music/Test Artist/Second Album/01 - Uno.flac
/// music/Test Artist/Second Album/Mix.pls       → /…/02 - Two.mp3, 01 - Uno.flac
/// music/Road Trip [playlist]/x.mp3
/// music/Lists/top.m3u8                         → ../Test Artist/First Album/02 - Two.mp3
/// ```
struct Fixture {
    _tmp: tempfile::TempDir,
    db: Database,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    vf: Arc<dyn VirtualFolderManager>,
    scanner: LibraryScanner,
    media: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("music");
        let first = media.join("Test Artist/First Album");
        let second = media.join("Test Artist/Second Album");
        for dir in [
            &first,
            &second,
            &media.join("Road Trip [playlist]"),
            &media.join("Lists"),
        ] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        for track in ["01 - One.mp3", "02 - Two.mp3", "03 - Three.mp3"] {
            std::fs::write(first.join(track), b"").expect("track");
        }
        std::fs::write(second.join("01 - Uno.flac"), b"").expect("track");
        std::fs::write(media.join("Road Trip [playlist]/x.mp3"), b"").expect("track");
        std::fs::write(
            first.join("Album.m3u"),
            "#EXTM3U\n03 - Three.mp3\n01 - One.mp3\n03 - Three.mp3\nmissing.mp3\n",
        )
        .expect("m3u");
        std::fs::write(
            second.join("Mix.pls"),
            format!(
                "[playlist]\nFile1={}\nFile2=01 - Uno.flac\nNumberOfEntries=2\nVersion=2\n",
                first.join("02 - Two.mp3").display()
            ),
        )
        .expect("pls");
        std::fs::write(
            media.join("Lists/top.m3u8"),
            "../Test Artist/First Album/02 - Two.mp3\n",
        )
        .expect("m3u8");

        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Music",
            Some(CollectionTypeOptions::music),
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
            Arc::clone(&vf),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_items(Arc::clone(&items));
        Self {
            _tmp: tmp,
            db,
            items,
            persistence,
            vf,
            scanner,
            media,
        }
    }

    async fn scan(&self) {
        self.scanner.scan_all().await.expect("scan");
    }

    fn path(&self, rel: &str) -> String {
        self.media.join(rel).to_string_lossy().into_owned()
    }

    fn id(&self, kind: BaseItemKind, rel: &str) -> Uuid {
        derive_item_id(kind, &self.path(rel)).expect("id")
    }

    async fn library(&self) -> String {
        let folders = self.vf.get_virtual_folders().await.expect("folders");
        let id = folders[0].item_id.as_deref().expect("library id");
        guid_to_db(Uuid::parse_str(id).expect("uuid"))
    }

    async fn row(&self, id: Uuid) -> Option<BaseItemEntity> {
        self.items.retrieve_item(id).await.expect("row")
    }

    /// The playlist at `rel`'s members, by their path relative to the
    /// library, in order.
    async fn members(&self, rel: &str) -> Vec<String> {
        let rows: Vec<Option<String>> = sqlx::query_scalar(
            r#"SELECT b."Path" FROM "LinkedChildren" l
               JOIN "BaseItems" b ON b."Id" = l."ChildId"
               WHERE l."ParentId" = ?1 ORDER BY l."SortOrder""#,
        )
        .bind(guid_to_db(self.id(BaseItemKind::Playlist, rel)))
        .fetch_all(self.db.pool())
        .await
        .expect("members");
        let root = format!("{}/", self.media.display());
        rows.into_iter()
            .map(|path| {
                path.expect("member path")
                    .trim_start_matches(&root)
                    .to_owned()
            })
            .collect()
    }

    /// Rewrites the playlist file at `rel`, its mtime moved on so the
    /// change is seen whatever the filesystem's timestamp resolution.
    fn rewrite(&self, rel: &str, contents: &str) {
        let path = self.media.join(rel);
        std::fs::write(&path, contents).expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(SystemTime::now() + Duration::from_secs(60))
            .expect("touch");
    }
}

const ALBUM: &str = "Test Artist/First Album";
const ALBUM_M3U: &str = "Test Artist/First Album/Album.m3u";
const MIX_PLS: &str = "Test Artist/Second Album/Mix.pls";
const TOP: &str = "Lists/top.m3u8";
const ROAD_TRIP: &str = "Road Trip [playlist]";
const ONE: &str = "Test Artist/First Album/01 - One.mp3";
const TWO: &str = "Test Artist/First Album/02 - Two.mp3";
const THREE: &str = "Test Artist/First Album/03 - Three.mp3";
const UNO: &str = "Test Artist/Second Album/01 - Uno.flac";

#[tokio::test]
async fn a_playlist_file_is_a_playlist_of_its_entries() {
    let f = Fixture::new().await;
    f.scan().await;
    let library = f.library().await;

    let album = f
        .row(f.id(BaseItemKind::Playlist, ALBUM_M3U))
        .await
        .expect("the album's playlist");
    assert_eq!(album.name.as_deref(), Some("Album"));
    assert_eq!(album.path.as_deref(), Some(f.path(ALBUM_M3U).as_str()));
    assert_eq!(
        album.parent_id.as_deref(),
        Some(guid_to_db(f.id(BaseItemKind::MusicAlbum, ALBUM)).as_str()),
        "under the album it sits in"
    );
    assert_eq!(album.top_parent_id.as_deref(), Some(library.as_str()));
    assert!(album.is_folder);
    assert!(album.is_in_mixed_folder);
    assert_eq!(album.media_type.as_deref(), Some("Audio"));
    // A file's dates and size, not a folder's.
    assert!(album.date_modified.is_some());
    assert!(album.size.is_some_and(|size| size > 0));
    let data: serde_json::Value =
        serde_json::from_str(album.data.as_deref().expect("data")).expect("json");
    assert_eq!(data["OpenAccess"], true);
    assert_eq!(data["OwnerUserId"], "00000000000000000000000000000000");
    assert_eq!(data["PlaylistMediaType"], "Audio");

    // Order and the repeat kept, the missing entry dropped.
    assert_eq!(f.members(ALBUM_M3U).await, [THREE, ONE, THREE]);
    // An absolute entry and a relative one.
    assert_eq!(f.members(MIX_PLS).await, [TWO, UNO]);
    // A `../` entry, from a folder that is neither artist nor album: the
    // playlist hangs off the library (Jellyfin's parent is a plain `Folder`
    // for `Lists`, a row a music scan here does not plan).
    assert_eq!(f.members(TOP).await, [TWO]);
    let top = f.row(f.id(BaseItemKind::Playlist, TOP)).await.expect("top");
    assert_eq!(top.parent_id.as_deref(), Some(library.as_str()));

    // Owned by nobody, open to everyone.
    let meta: (String, bool) = sqlx::query_as(
        r#"SELECT "OwnerUserId", "OpenAccess" FROM "FerrofinPlaylists" WHERE "PlaylistId" = ?1"#,
    )
    .bind(&album.id)
    .fetch_one(f.db.pool())
    .await
    .expect("ownership row");
    assert_eq!(meta, (guid_to_db(Uuid::nil()), true));
}

#[tokio::test]
async fn a_pls_is_no_track_and_a_playlist_folder_holds_no_items() {
    let f = Fixture::new().await;
    // An older scan made a track of the `.pls`.
    let stale = f.id(BaseItemKind::Audio, MIX_PLS);
    f.persistence
        .save_items(&[BaseItemEntity {
            id: guid_to_db(stale),
            type_: "MediaBrowser.Controller.Entities.Audio.Audio".to_owned(),
            name: Some("Mix".to_owned()),
            path: Some(f.path(MIX_PLS)),
            parent_id: Some(f.library().await),
            top_parent_id: Some(f.library().await),
            media_type: Some("Audio".to_owned()),
            ..BaseItemEntity::default()
        }])
        .await
        .expect("seed the stale track");
    f.scan().await;

    assert!(f.row(stale).await.is_none(), "the stale track is pruned");
    assert!(f.row(f.id(BaseItemKind::Playlist, MIX_PLS)).await.is_some());

    let road_trip = f
        .row(f.id(BaseItemKind::Playlist, ROAD_TRIP))
        .await
        .expect("the playlist folder");
    assert_eq!(road_trip.name.as_deref(), Some("Road Trip"));
    assert_eq!(road_trip.media_type.as_deref(), Some("Unknown"));
    assert!(!road_trip.is_in_mixed_folder);
    assert_eq!(road_trip.date_modified, None, "a folder has no mtime");
    assert!(f.members(ROAD_TRIP).await.is_empty());
    let inside = f.id(BaseItemKind::Audio, "Road Trip [playlist]/x.mp3");
    assert!(f.row(inside).await.is_none(), "nothing in it is an item");
    assert!(
        f.row(f.id(BaseItemKind::MusicAlbum, ROAD_TRIP))
            .await
            .is_none(),
        "nor is it an album"
    );
}

#[tokio::test]
async fn a_rescan_follows_the_file() {
    let f = Fixture::new().await;
    f.scan().await;
    assert_eq!(f.members(ALBUM_M3U).await, [THREE, ONE, THREE]);

    // Unchanged: nothing is read again, so members set elsewhere stay.
    sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "SortOrder" = 2"#)
        .bind(guid_to_db(f.id(BaseItemKind::Playlist, ALBUM_M3U)))
        .execute(f.db.writer())
        .await
        .expect("drop one");
    f.scan().await;
    assert_eq!(f.members(ALBUM_M3U).await, [THREE, ONE]);

    // Edited: the file's entries replace them.
    f.rewrite(ALBUM_M3U, "02 - Two.mp3\n01 - One.mp3\n");
    f.scan().await;
    assert_eq!(f.members(ALBUM_M3U).await, [TWO, ONE]);

    // A full refresh that keeps metadata puts the file's entries first and
    // keeps the stored ones not among them (`DistinctBy(ItemId)`).
    f.rewrite(ALBUM_M3U, "03 - Three.mp3\n02 - Two.mp3\n");
    f.scanner
        .scan_with(
            None,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                ..MetadataRefreshOptions::default()
            },
        )
        .await
        .expect("full refresh");
    assert_eq!(f.members(ALBUM_M3U).await, [THREE, TWO, ONE]);

    // Gone: the playlist goes with it.
    std::fs::remove_file(f.media.join(ALBUM_M3U)).expect("rm");
    f.scan().await;
    assert!(
        f.row(f.id(BaseItemKind::Playlist, ALBUM_M3U))
            .await
            .is_none()
    );
    assert!(f.members(ALBUM_M3U).await.is_empty());
}

/// A library of `collection_type` over `media`, scanned once.
async fn scanned(collection_type: CollectionTypeOptions, media: &std::path::Path) -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(media.with_file_name("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Library",
        Some(collection_type),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .scan_all()
        .await
        .expect("scan");
    db
}

/// The row stored at `id`, when there is one.
async fn stored(db: &Database, id: Uuid) -> Option<BaseItemEntity> {
    sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .expect("query")
}

/// The file arm is for music and untyped libraries: a movie library leaves
/// a playlist file alone.
#[tokio::test]
async fn a_movie_library_has_no_playlist_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    std::fs::create_dir_all(media.join("Heat (1995)")).expect("mkdir");
    std::fs::write(media.join("Heat (1995)/Heat (1995).mkv"), b"").expect("movie");
    std::fs::write(media.join("list.m3u"), "Heat (1995)/Heat (1995).mkv\n").expect("m3u");
    let db = scanned(CollectionTypeOptions::movies, &media).await;
    let playlist = derive_item_id(
        BaseItemKind::Playlist,
        &media.join("list.m3u").to_string_lossy(),
    )
    .expect("id");
    assert!(stored(&db, playlist).await.is_none());
}

/// An untyped library's playlist file is a playlist under the folder it
/// sits in. Its entries resolve only in libraries with a collection type
/// (`PlaylistItemsProvider`'s roots), so one naming the untyped library's
/// own files has no members, as upstream.
#[tokio::test]
async fn an_untyped_library_plans_its_playlist_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("mixed");
    std::fs::create_dir_all(media.join("Mixes")).expect("mkdir");
    std::fs::write(media.join("song.mp3"), b"").expect("song");
    std::fs::write(media.join("Mixes/list.m3u"), "../song.mp3\n").expect("m3u");
    let db = scanned(CollectionTypeOptions::mixed, &media).await;
    let path = media.join("Mixes/list.m3u").to_string_lossy().into_owned();
    let playlist = stored(
        &db,
        derive_item_id(BaseItemKind::Playlist, &path).expect("id"),
    )
    .await
    .expect("the playlist");
    let folder =
        derive_item_id(BaseItemKind::Folder, &media.join("Mixes").to_string_lossy()).expect("id");
    assert_eq!(playlist.parent_id, Some(guid_to_db(folder)));
    assert_eq!(playlist.media_type.as_deref(), Some("Audio"));
    let members: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "LinkedChildren" WHERE "ParentId" = ?1"#)
            .bind(&playlist.id)
            .fetch_one(db.pool())
            .await
            .expect("count");
    assert_eq!(members, 0);
}

/// Rows an older scan planned inside a folder that now resolves as a
/// playlist folder — an album of its track — are pruned: nothing in a
/// playlist folder is an item.
#[tokio::test]
async fn an_album_inside_a_new_playlist_folder_is_pruned() {
    let f = Fixture::new().await;
    let library = f.library().await;
    let album = f.id(BaseItemKind::MusicAlbum, ROAD_TRIP);
    let track = f.id(BaseItemKind::Audio, "Road Trip [playlist]/x.mp3");
    f.persistence
        .save_items(&[
            BaseItemEntity {
                id: guid_to_db(album),
                type_: "MediaBrowser.Controller.Entities.Audio.MusicAlbum".to_owned(),
                name: Some("Road Trip [playlist]".to_owned()),
                path: Some(f.path(ROAD_TRIP)),
                parent_id: Some(library.clone()),
                top_parent_id: Some(library.clone()),
                is_folder: true,
                ..BaseItemEntity::default()
            },
            BaseItemEntity {
                id: guid_to_db(track),
                type_: "MediaBrowser.Controller.Entities.Audio.Audio".to_owned(),
                name: Some("x".to_owned()),
                path: Some(f.path("Road Trip [playlist]/x.mp3")),
                parent_id: Some(guid_to_db(album)),
                top_parent_id: Some(library),
                media_type: Some("Audio".to_owned()),
                ..BaseItemEntity::default()
            },
        ])
        .await
        .expect("seed the old album");
    f.scan().await;
    assert!(f.row(album).await.is_none(), "the old album is pruned");
    assert!(f.row(track).await.is_none(), "and its track");
    assert!(
        f.row(f.id(BaseItemKind::Playlist, ROAD_TRIP))
            .await
            .is_some()
    );
}

/// A playlist folder takes its members, and its media type, from its
/// `playlist.xml` — read again when the file changes, and only then.
#[tokio::test]
async fn a_playlist_folder_reads_its_playlist_xml_when_it_changes() {
    let f = Fixture::new().await;
    let xml = |paths: &[&str]| {
        let items = paths.iter().fold(String::new(), |mut out, rel| {
            let _ = write!(
                out,
                "<PlaylistItem><Path>{}</Path></PlaylistItem>",
                f.path(rel)
            );
            out
        });
        format!(
            "<?xml version=\"1.0\"?><Item><PlaylistMediaType>Audio</PlaylistMediaType>\
             <PlaylistItems>{items}</PlaylistItems></Item>"
        )
    };
    let file = f.media.join("Road Trip [playlist]/playlist.xml");
    std::fs::write(&file, xml(&[ONE, TWO])).expect("xml");
    f.scan().await;
    assert_eq!(f.members(ROAD_TRIP).await, [ONE, TWO]);
    let row = f
        .row(f.id(BaseItemKind::Playlist, ROAD_TRIP))
        .await
        .expect("row");
    assert_eq!(row.media_type.as_deref(), Some("Audio"));

    // Edited: read again. The mtime is set from the fine clock: the one the
    // filesystem stamps may trail the scan's saved stamp by a tick.
    let edit = |paths: &[&str]| {
        std::fs::write(&file, xml(paths)).expect("xml");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .expect("open")
            .set_modified(SystemTime::now())
            .expect("touch");
    };
    edit(&[TWO]);
    f.scan().await;
    assert_eq!(f.members(ROAD_TRIP).await, [TWO]);

    // Unchanged since: not read again, so members set elsewhere stay.
    sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "SortOrder" > 0"#)
        .bind(&row.id)
        .execute(f.db.writer())
        .await
        .expect("trim");
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, 1, ?2, 0)"#,
    )
    .bind(&row.id)
    .bind(guid_to_db(f.id(BaseItemKind::Audio, UNO)))
    .execute(f.db.writer())
    .await
    .expect("add");
    f.scan().await;
    assert_eq!(f.members(ROAD_TRIP).await, [TWO, UNO]);

    // An XML that reads is metadata whatever its entries name
    // (`BaseXmlProvider`): one naming nothing stored empties the playlist,
    // and is not read again after.
    edit(&["missing.mp3"]);
    f.scan().await;
    assert!(f.members(ROAD_TRIP).await.is_empty());
    let saved = f
        .row(f.id(BaseItemKind::Playlist, ROAD_TRIP))
        .await
        .expect("row")
        .date_last_saved
        .expect("saved");
    let modified = chrono::DateTime::<chrono::Utc>::from(
        std::fs::metadata(&file)
            .expect("stat")
            .modified()
            .expect("mtime"),
    );
    assert!(saved >= modified, "{saved} < {modified}");
}

/// A full refresh that keeps metadata merges the file's entries into the
/// stored members — but takes them whole, repeats and all, when there are
/// none stored (`replaceData || LinkedChildren.Length == 0`).
#[tokio::test]
async fn a_full_refresh_of_an_empty_playlist_keeps_its_repeats() {
    let f = Fixture::new().await;
    f.scan().await;
    sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1"#)
        .bind(guid_to_db(f.id(BaseItemKind::Playlist, ALBUM_M3U)))
        .execute(f.db.writer())
        .await
        .expect("empty it");
    f.scanner
        .scan_with(
            None,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                ..MetadataRefreshOptions::default()
            },
        )
        .await
        .expect("full refresh");
    assert_eq!(f.members(ALBUM_M3U).await, [THREE, ONE, THREE]);
}

/// What the file says replaces nothing the store cannot find: entries that
/// name no item leave the members as they were (`HasMetadata` false), and a
/// playlist's ownership row, once there, is never rewritten.
#[tokio::test]
async fn a_rescan_keeps_what_the_file_cannot_replace() {
    let f = Fixture::new().await;
    f.scan().await;
    let id = guid_to_db(f.id(BaseItemKind::Playlist, ALBUM_M3U));
    let owner = guid_to_db(Uuid::new_v4());
    sqlx::query(
        r#"UPDATE "FerrofinPlaylists" SET "OwnerUserId" = ?2, "OpenAccess" = 0
           WHERE "PlaylistId" = ?1"#,
    )
    .bind(&id)
    .bind(&owner)
    .execute(f.db.writer())
    .await
    .expect("owner");
    std::fs::write(f.media.join(ALBUM).join("notes.txt"), b"").expect("notes");
    f.rewrite(ALBUM_M3U, "notes.txt\nmissing.mp3\n");
    f.scan().await;
    assert_eq!(f.members(ALBUM_M3U).await, [THREE, ONE, THREE]);
    let meta: (String, bool) = sqlx::query_as(
        r#"SELECT "OwnerUserId", "OpenAccess" FROM "FerrofinPlaylists" WHERE "PlaylistId" = ?1"#,
    )
    .bind(&id)
    .fetch_one(f.db.pool())
    .await
    .expect("meta");
    assert_eq!(meta, (owner, false));
}

/// The `playlist.xml` Jellyfin writes beside a `playlist.m3u` is that file's
/// sidecar, not a playlist folder's own: the album holding both stays an
/// album, its tracks with it.
#[tokio::test]
async fn a_playlist_files_sidecar_leaves_its_album_an_album() {
    let f = Fixture::new().await;
    let dir = f.media.join(ALBUM);
    std::fs::write(dir.join("playlist.m3u"), "01 - One.mp3\n").expect("m3u");
    std::fs::write(
        dir.join("playlist.xml"),
        "<?xml version=\"1.0\"?><Item><LocalTitle>playlist</LocalTitle></Item>",
    )
    .expect("sidecar");
    f.scan().await;
    assert!(f.row(f.id(BaseItemKind::MusicAlbum, ALBUM)).await.is_some());
    assert!(f.row(f.id(BaseItemKind::Audio, ONE)).await.is_some());
    assert!(f.row(f.id(BaseItemKind::Playlist, ALBUM)).await.is_none());
    assert_eq!(f.members(&format!("{ALBUM}/playlist.m3u")).await, [ONE]);
}
