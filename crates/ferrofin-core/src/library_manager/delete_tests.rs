//! `delete_item` with `DeleteFileLocation` — upstream's
//! `LibraryManager.DeleteItem` (`LibraryManager.cs:421-625`) with the file
//! deletion `DELETE /Items/{itemId}` asks for, over real repositories and
//! real files in a temp dir. Each test pins upstream's behaviour, surprises
//! included (owner decision 2026-10-04: match Jellyfin exactly).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::DeleteOptions;
use ferrofin_traits::persistence::{ItemPersistenceService, LinkedChildrenService as _};
use ferrofin_traits::system::{ExternalDataManager, MediaLocation};
use uuid::Uuid;

use super::FerrofinLibraryManager;
use crate::item_count_service::FerrofinItemCountService;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::ItemTypeLookup;
use crate::people_repository::FerrofinPeopleRepository;
use crate::test_support::test_db;

/// A temp dir holding one library folder (`media/`) and the server's data
/// directory (`server/`).
struct Fixture {
    db: Database,
    manager: FerrofinLibraryManager,
    library: PathBuf,
    root: tempfile::TempDir,
    external: Arc<RecordingExternalData>,
}

/// Records each `DeleteExternalItemFiles` call: the id, the path and the
/// containing folder it was given, and whether the item's row was still
/// stored then.
#[derive(Default)]
struct RecordingExternalData {
    deleted: Mutex<Vec<(Uuid, String, String)>>,
    db: std::sync::OnceLock<Database>,
    row_stored: Mutex<Vec<(Uuid, bool)>>,
}

#[async_trait]
impl ExternalDataManager for RecordingExternalData {
    async fn delete_external_item_data(
        &self,
        _item_id: Uuid,
        _media: MediaLocation<'_>,
    ) -> Result<(), ServiceError> {
        unimplemented!("a delete only removes the files")
    }

    async fn delete_external_item_files(
        &self,
        item_id: Uuid,
        media: MediaLocation<'_>,
    ) -> Result<(), ServiceError> {
        self.deleted.lock().expect("lock").push((
            item_id,
            media.path.to_owned(),
            media.containing_folder.to_owned(),
        ));
        if let Some(db) = self.db.get() {
            let stored = crate::test_support::fetch_item_opt(db, item_id)
                .await
                .is_some();
            self.row_stored
                .lock()
                .expect("lock")
                .push((item_id, stored));
        }
        Ok(())
    }
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tmp");
    let library = root.path().join("media");
    std::fs::create_dir_all(&library).expect("library");
    let db = test_db().await;
    let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
        root.path().join("server"),
        root.path().join("server/log"),
        root.path().join("server/config"),
        root.path().join("server/cache"),
        root.path().join("server/web"),
    ));
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    let manager = FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    )
    .with_app_paths(paths)
    .with_linked_children(Arc::new(
        crate::linked_children_service::FerrofinLinkedChildrenService::new(db.clone()),
    ));
    let external = Arc::new(RecordingExternalData::default());
    let _ = external.db.set(db.clone());
    manager.set_external_data(external.clone());
    Fixture {
        db,
        manager,
        library,
        root,
        external,
    }
}

/// Writes a small file (and its directories).
fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, b"media").expect("write");
}

/// Stores a row of `kind` at `path`, shaped by `edit`.
async fn row(
    db: &Database,
    kind: BaseItemKind,
    path: Option<&Path>,
    edit: impl FnOnce(&mut BaseItemEntity),
) -> Uuid {
    let id = Uuid::new_v4();
    let mut entity = BaseItemEntity {
        id: guid_to_db(id),
        type_: kind.stored_type_name().unwrap_or_default().to_owned(),
        name: Some(format!("{kind:?}")),
        path: path.map(|p| p.to_string_lossy().into_owned()),
        is_folder: crate::kinds::is_folder(kind),
        ..BaseItemEntity::default()
    };
    edit(&mut entity);
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[entity])
        .await
        .expect("save");
    id
}

async fn exists(fixture: &Fixture, id: Uuid) -> bool {
    fixture
        .manager
        .get_item_by_id(id)
        .await
        .expect("read")
        .is_some()
}

fn with_files() -> DeleteOptions {
    DeleteOptions {
        delete_file_location: true,
        ..DeleteOptions::default()
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `BaseItem.GetDeletePaths` for a file in a mixed folder: the file, then
/// every sidecar whose name starts with its own — `Aliens.nfo` included —
/// and, before the rows, its extracted data (`DeleteExternalItemFiles`) with
/// the file's directory as its containing folder.
#[tokio::test]
async fn a_mixed_folder_movie_takes_every_prefix_sidecar() {
    let f = fixture().await;
    for name in [
        "Alien.mkv",
        "Alien.nfo",
        "Alien-poster.jpg",
        "Alien.en.srt",
        "Aliens.mkv",
        "Aliens.nfo",
        "Predator.nfo",
    ] {
        touch(&f.library.join(name));
    }
    let file = f.library.join("Alien.mkv");
    let alien = row(&f.db, BaseItemKind::Movie, Some(&file), |e| {
        e.is_in_mixed_folder = true;
    })
    .await;
    let aliens = row(
        &f.db,
        BaseItemKind::Movie,
        Some(&f.library.join("Aliens.mkv")),
        |e| e.is_in_mixed_folder = true,
    )
    .await;

    f.manager
        .delete_item(alien, &with_files())
        .await
        .expect("delete");

    assert!(!exists(&f, alien).await);
    assert!(exists(&f, aliens).await);
    assert_eq!(
        names(&f.library),
        ["Aliens.mkv", "Predator.nfo"],
        "upstream's prefix match takes Aliens.nfo too"
    );
    let path = file.to_string_lossy().into_owned();
    let folder = f.library.to_string_lossy().into_owned();
    assert_eq!(
        f.external.deleted.lock().expect("lock").clone(),
        // `DeleteExternalItemFiles` once, after the files, before the rows:
        // it also covers what upstream's `GetInternalMetadataPaths` lists for
        // a video.
        vec![(alien, path, folder)]
    );
    assert_eq!(
        f.external.row_stored.lock().expect("lock").clone(),
        vec![(alien, true)],
        "the extracted data goes while the row is still stored (`:597-604`)"
    );
}

/// `Video.GetDeletePaths` (`Video.cs:727-742`): a movie not in a mixed
/// folder takes its whole `ContainingFolderPath`, extras and other versions
/// inside it included; the rows the persistence delete takes (the movie and
/// the extras it owns) go, and its internal metadata folder goes first.
#[tokio::test]
async fn a_movie_in_its_own_folder_takes_the_whole_folder() {
    let f = fixture().await;
    let folder = f.library.join("Film (2000)");
    let movie_file = folder.join("Film (2000).mkv");
    let trailer_file = folder.join("Film (2000)-trailer.mkv");
    for path in [&movie_file, &trailer_file] {
        touch(path);
    }
    touch(&folder.join("Film (2000) - 4K.mkv"));
    touch(&folder.join("extras/making of.mkv"));
    let movie = row(&f.db, BaseItemKind::Movie, Some(&movie_file), |_| {}).await;
    let trailer = row(&f.db, BaseItemKind::Trailer, Some(&trailer_file), |e| {
        e.owner_id = Some(guid_to_db(movie));
        e.extra_type = Some(1);
    })
    .await;
    let metadata = crate::path_manager::item_internal_metadata_path(
        &f.root.path().join("server/metadata").to_string_lossy(),
        movie,
    );
    touch(&metadata.join("poster.jpg"));
    // Ferrofin's own art folder, `{metadata}/library/{GUID}`.
    let art = f
        .root
        .path()
        .join("server/metadata/library")
        .join(guid_to_db(movie));
    touch(&art.join("poster.jpg"));

    f.manager
        .delete_item(movie, &with_files())
        .await
        .expect("delete");

    assert!(!folder.exists(), "the movie's whole folder is deleted");
    assert!(!exists(&f, movie).await);
    assert!(!exists(&f, trailer).await, "the owned extra's row goes too");
    assert!(!metadata.exists(), "the internal metadata folder goes too");
    assert!(!art.exists(), "and Ferrofin's art folder");
}

/// Upstream resolves a suffix extra beside its film with `IsInMixedFolder =
/// false` (`LibraryManager.FindExtras`, `AddCandidate(current, …, false)`),
/// so its `Video.GetDeletePaths` names the film's folder: deleting the
/// trailer deletes the film's files. Kept as upstream does it; the film's row
/// stays until a scan finds its file gone.
#[tokio::test]
async fn deleting_an_extra_beside_its_film_takes_the_films_folder_as_upstream() {
    let f = fixture().await;
    let folder = f.library.join("Film (2000)");
    let movie_file = folder.join("Film (2000).mkv");
    let trailer_file = folder.join("Film (2000)-trailer.mkv");
    touch(&movie_file);
    touch(&trailer_file);
    let movie = row(&f.db, BaseItemKind::Movie, Some(&movie_file), |_| {}).await;
    let trailer = row(&f.db, BaseItemKind::Trailer, Some(&trailer_file), |e| {
        e.owner_id = Some(guid_to_db(movie));
        e.extra_type = Some(1);
    })
    .await;

    f.manager
        .delete_item(trailer, &with_files())
        .await
        .expect("delete");

    assert!(!exists(&f, trailer).await);
    assert!(!folder.exists(), "the film's folder goes with its trailer");
    assert!(exists(&f, movie).await, "the film's row stays");
}

/// A folder item's path is its directory, removed with everything in it,
/// and its children's rows and metadata go with it; an episode takes only
/// its own file (`Episode.GetDeletePaths`).
#[tokio::test]
async fn a_series_takes_its_directory_and_an_episode_only_its_file() {
    let f = fixture().await;
    let series_dir = f.library.join("Show");
    let first = series_dir.join("Season 1/Show S01E01.mkv");
    let second = series_dir.join("Season 1/Show S01E02.mkv");
    touch(&first);
    touch(&second);
    let series = row(&f.db, BaseItemKind::Series, Some(&series_dir), |_| {}).await;
    let season = row(
        &f.db,
        BaseItemKind::Season,
        Some(&series_dir.join("Season 1")),
        |e| e.parent_id = Some(guid_to_db(series)),
    )
    .await;
    let episode_one = row(&f.db, BaseItemKind::Episode, Some(&first), |e| {
        e.parent_id = Some(guid_to_db(season));
    })
    .await;
    let episode_two = row(&f.db, BaseItemKind::Episode, Some(&second), |e| {
        e.parent_id = Some(guid_to_db(season));
    })
    .await;

    f.manager
        .delete_item(episode_one, &with_files())
        .await
        .expect("delete episode");
    assert!(!first.exists());
    assert!(second.is_file(), "the season folder stays");

    f.manager
        .delete_item(series, &with_files())
        .await
        .expect("delete series");
    assert!(!series_dir.exists());
    for id in [series, season, episode_two] {
        assert!(!exists(&f, id).await);
    }
    // The children's extracted data is cleaned before the rows, the season
    // with its own folder as its containing folder.
    let recorded = f.external.deleted.lock().expect("lock").clone();
    let season_dir = series_dir.join("Season 1").to_string_lossy().into_owned();
    assert!(
        recorded.iter().any(|(id, path, folder)| *id == season
            && *path == season_dir
            && *folder == season_dir),
        "{recorded:?}"
    );
}

/// Whether this process ignores permission bits (root), where a read-only
/// directory cannot be simulated with `chmod`.
fn permissions_are_bypassed() -> bool {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `DeleteItemPath` rethrows the first path's failure: the delete stops with
/// every row in place and nothing of the item deleted — after upstream has
/// already removed the item's internal metadata (`GetMetadataPaths` runs
/// first).
#[tokio::test]
async fn a_first_path_that_cannot_be_deleted_fails_the_delete_with_the_rows_kept() {
    use std::os::unix::fs::PermissionsExt as _;
    if permissions_are_bypassed() {
        return;
    }
    let f = fixture().await;
    let folder = f.library.join("Film (2000)");
    let movie_file = folder.join("Film (2000).mkv");
    touch(&movie_file);
    let movie = row(&f.db, BaseItemKind::Movie, Some(&movie_file), |_| {}).await;
    let mixed_dir = f.library.join("Loose");
    let mixed_file = mixed_dir.join("Loose.mkv");
    touch(&mixed_file);
    touch(&mixed_dir.join("Loose.nfo"));
    let mixed = row(&f.db, BaseItemKind::Movie, Some(&mixed_file), |e| {
        e.is_in_mixed_folder = true;
    })
    .await;
    let metadata = crate::path_manager::item_internal_metadata_path(
        &f.root.path().join("server/metadata").to_string_lossy(),
        movie,
    );
    touch(&metadata.join("poster.jpg"));

    // A folder whose entries cannot be removed, as on a read-only mount.
    for (locked, item, files) in [
        (folder.clone(), movie, vec![movie_file.clone()]),
        (
            mixed_dir.clone(),
            mixed,
            vec![mixed_file.clone(), mixed_dir.join("Loose.nfo")],
        ),
    ] {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let err = f
            .manager
            .delete_item(item, &with_files())
            .await
            .expect_err("the first path cannot be deleted");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(matches!(err, ServiceError::Backend(_)), "{err}");
        assert!(exists(&f, item).await, "the row stays");
        for file in files {
            assert!(file.is_file(), "{} stays", file.display());
        }
    }
    assert!(!metadata.exists(), "upstream deletes the metadata first");
}

/// The files go before the rows: a row delete that fails afterwards leaves
/// the rows with the files already gone, as upstream's order does.
#[tokio::test]
async fn the_files_go_before_the_rows() {
    let f = fixture().await;
    let folder = f.library.join("Film (2000)");
    let file = folder.join("Film (2000).mkv");
    touch(&file);
    let movie = row(&f.db, BaseItemKind::Movie, Some(&file), |_| {}).await;
    crate::item_persistence_service::fail_deletes_of(&f.db, movie).await;

    let err = f
        .manager
        .delete_item(movie, &with_files())
        .await
        .expect_err("the rows cannot be deleted");

    assert!(!matches!(err, ServiceError::Unauthorized(_)), "{err}");
    assert!(exists(&f, movie).await);
    assert!(!folder.exists());
}

#[tokio::test]
async fn without_delete_file_location_only_the_rows_go() {
    let f = fixture().await;
    let file = f.library.join("Film (2000)/Film (2000).mkv");
    touch(&file);
    let movie = row(&f.db, BaseItemKind::Movie, Some(&file), |_| {}).await;

    f.manager
        .delete_item(movie, &DeleteOptions::default())
        .await
        .expect("delete");

    assert!(!exists(&f, movie).await);
    assert!(file.is_file());
}

/// "File not found, only removing from database".
#[tokio::test]
async fn a_missing_file_still_deletes_the_row() {
    let f = fixture().await;
    let movie = row(
        &f.db,
        BaseItemKind::Movie,
        Some(&f.library.join("Gone (1999)/Gone (1999).mkv")),
        |_| {},
    )
    .await;
    f.manager
        .delete_item(movie, &with_files())
        .await
        .expect("delete");
    assert!(!exists(&f, movie).await);
}

#[tokio::test]
async fn collections_delete_their_own_folder_in_the_data_directory() {
    let f = fixture().await;
    // A collection Ferrofin created has no folder of its own.
    let ours = row(&f.db, BaseItemKind::BoxSet, None, |_| {}).await;
    f.manager
        .delete_item(ours, &with_files())
        .await
        .expect("delete");
    assert!(!exists(&f, ours).await);

    // An adopted Jellyfin collection is stored under `%AppDataPath%`.
    let folder = f.root.path().join("server/data/collections/Saga [boxset]");
    touch(&folder.join("collection.xml"));
    let adopted = row(&f.db, BaseItemKind::BoxSet, None, |e| {
        e.path = Some("%AppDataPath%/collections/Saga [boxset]".to_owned());
    })
    .await;
    f.manager
        .delete_item(adopted, &with_files())
        .await
        .expect("delete");
    assert!(!exists(&f, adopted).await);
    assert!(!folder.exists());
    assert!(f.root.path().join("server/data/collections").is_dir());
}

#[tokio::test]
async fn a_streamed_or_channel_item_deletes_no_file() {
    let f = fixture().await;
    let streamed = row(&f.db, BaseItemKind::Movie, None, |e| {
        e.path = Some("https://cdn.example/film.mkv".to_owned());
    })
    .await;
    let file = f.library.join("Channel (2001)/Channel (2001).mkv");
    touch(&file);
    let channel_item = row(&f.db, BaseItemKind::Movie, Some(&file), |e| {
        e.channel_id = Some(guid_to_db(Uuid::from_u128(0xC1)));
    })
    .await;
    for id in [streamed, channel_item] {
        f.manager
            .delete_item(id, &with_files())
            .await
            .expect("delete");
        assert!(!exists(&f, id).await);
    }
    assert!(file.is_file(), "a channel item's media is its channel's");
}

/// The linked-children service the fixture's manager reroutes through.
fn linked(db: &Database) -> crate::linked_children_service::FerrofinLinkedChildrenService {
    crate::linked_children_service::FerrofinLinkedChildrenService::new(db.clone())
}

/// A `LinkedChildren` row: `child` listed under `parent` as `child_type`,
/// appended after the parent's other rows.
async fn link(db: &Database, parent: Uuid, child: Uuid, child_type: i32) {
    linked(db)
        .upsert_linked_child(parent, child, child_type)
        .await
        .expect("link");
}

/// The `(child, type)` rows under `parent`: manual, local-version, then
/// linked-version rows, each in `SortOrder`.
async fn links_of(db: &Database, parent: Uuid) -> Vec<(Uuid, i32)> {
    let mut out = Vec::new();
    for child_type in [0, 2, 3] {
        for child in linked(db)
            .get_linked_children_ids(parent, Some(child_type))
            .await
            .expect("links")
        {
            out.push((child, child_type));
        }
    }
    out
}

/// A film with two local versions beside it, a merged version elsewhere, a
/// stacked part and a trailer, and a playlist that lists the film.
struct VersionGroup {
    folder: PathBuf,
    primary: Uuid,
    first: Uuid,
    second: Uuid,
    merged: Uuid,
    part: Uuid,
    trailer: Uuid,
    playlist: Uuid,
    second_path: String,
}

async fn version_group(f: &Fixture) -> VersionGroup {
    let folder = f.library.join("Film (2000)");
    let primary_file = folder.join("Film (2000) - 1080p.mkv");
    let first_file = folder.join("Film (2000) - 4K.mkv");
    let second_file = folder.join("Film (2000) - 720p.mkv");
    let part_file = folder.join("Film (2000) - 1080p-cd2.mkv");
    let trailer_file = folder.join("Film (2000)-trailer.mkv");
    let merged_file = f.library.join("Elsewhere/Film.mkv");
    for path in [
        &primary_file,
        &first_file,
        &second_file,
        &part_file,
        &trailer_file,
        &merged_file,
    ] {
        touch(path);
    }
    let first_path = first_file.to_string_lossy().into_owned();
    let second_path = second_file.to_string_lossy().into_owned();
    let data = serde_json::json!({
        "LocalAlternateVersions": [first_path, second_path.clone()],
        "AdditionalParts": [part_file.to_string_lossy()],
    })
    .to_string();
    let primary = row(&f.db, BaseItemKind::Movie, Some(&primary_file), |e| {
        e.data = Some(data);
    })
    .await;
    let primary_key = primary.as_simple().to_string();
    let version = |e: &mut BaseItemEntity| {
        e.owner_id = Some(guid_to_db(primary));
        e.primary_version_id = Some(guid_to_db(primary));
        e.presentation_unique_key = Some(primary_key.clone());
    };
    let first = row(&f.db, BaseItemKind::Movie, Some(&first_file), version).await;
    let second = row(&f.db, BaseItemKind::Movie, Some(&second_file), version).await;
    let merged = row(&f.db, BaseItemKind::Movie, Some(&merged_file), |e| {
        e.primary_version_id = Some(guid_to_db(primary));
        e.presentation_unique_key = Some(primary_key.clone());
    })
    .await;
    let part = row(&f.db, BaseItemKind::Video, Some(&part_file), |e| {
        e.owner_id = Some(guid_to_db(primary));
    })
    .await;
    let trailer = row(&f.db, BaseItemKind::Trailer, Some(&trailer_file), |e| {
        e.owner_id = Some(guid_to_db(primary));
        e.extra_type = Some(1);
    })
    .await;
    link(&f.db, primary, first, 2).await;
    link(&f.db, primary, second, 2).await;
    link(&f.db, primary, merged, 3).await;
    let playlist = row(&f.db, BaseItemKind::Playlist, None, |_| {}).await;
    link(&f.db, playlist, primary, 0).await;
    VersionGroup {
        folder,
        primary,
        first,
        second,
        merged,
        part,
        trailer,
        playlist,
        second_path,
    }
}

/// `LibraryManager.DeleteItem` of a primary video (`LibraryManager.cs:
/// 461-537`): the first remaining local version takes its place — no
/// version and owned by nothing, its own presentation key, the old
/// primary's other versions under it — the other versions point at it (the
/// local one owned by it, the merged one by nothing), the playlist entry
/// names it, and its user data stays. The stacked part and the trailer
/// stay the old primary's and go with it.
#[tokio::test]
async fn deleting_a_primary_promotes_its_first_version() {
    let f = fixture().await;
    let g = version_group(&f).await;
    let user = Uuid::new_v4();
    crate::test_support::seed_named_user(&f.db, user, "watcher").await;
    crate::test_support::seed_user_data(&f.db, user, g.first, true, None).await;

    f.manager
        .delete_item(g.primary, &DeleteOptions::default())
        .await
        .expect("delete");

    for gone in [g.primary, g.part, g.trailer] {
        assert!(!exists(&f, gone).await, "{gone} goes with the primary");
    }
    let new_key = g.first.as_simple().to_string();
    let promoted = crate::test_support::fetch_item(&f.db, g.first).await;
    assert_eq!(promoted.owner_id, None);
    assert_eq!(promoted.primary_version_id, None);
    assert_eq!(
        promoted.presentation_unique_key.as_deref(),
        Some(new_key.as_str())
    );
    assert_eq!(
        crate::video_versions::local_alternate_versions(promoted.data.as_deref()),
        vec![g.second_path.clone()]
    );
    let second = crate::test_support::fetch_item(&f.db, g.second).await;
    assert_eq!(
        second.owner_id,
        Some(guid_to_db(g.first)),
        "a local version"
    );
    assert_eq!(second.primary_version_id, Some(guid_to_db(g.first)));
    assert_eq!(
        second.presentation_unique_key.as_deref(),
        Some(new_key.as_str())
    );
    let merged = crate::test_support::fetch_item(&f.db, g.merged).await;
    assert_eq!(
        merged.owner_id, None,
        "a linked version is owned by nothing"
    );
    assert_eq!(merged.primary_version_id, Some(guid_to_db(g.first)));
    assert_eq!(
        links_of(&f.db, g.first).await,
        vec![(g.second, 2), (g.merged, 3)]
    );
    assert_eq!(
        links_of(&f.db, g.playlist).await,
        vec![(g.first, 0)],
        "the playlist entry is rerouted to the new primary"
    );
    assert_eq!(
        crate::user_data_manager::user_data_row_count(&f.db, g.first).await,
        1,
        "the promoted version keeps its user data"
    );
}

/// A version whose file is gone is deleted (its row only, no file), and the
/// next one is promoted (`missingAlternates`, `LibraryManager.cs:468-500`).
#[tokio::test]
async fn a_version_whose_file_is_gone_is_deleted_and_the_next_promoted() {
    let f = fixture().await;
    let g = version_group(&f).await;
    std::fs::remove_file(g.folder.join("Film (2000) - 4K.mkv")).expect("remove");

    f.manager
        .delete_item(g.primary, &DeleteOptions::default())
        .await
        .expect("delete");

    assert!(!exists(&f, g.first).await, "the missing version's row goes");
    let promoted = crate::test_support::fetch_item(&f.db, g.second).await;
    assert_eq!(promoted.primary_version_id, None);
    assert_eq!(promoted.owner_id, None);
    let merged = crate::test_support::fetch_item(&f.db, g.merged).await;
    assert_eq!(merged.primary_version_id, Some(guid_to_db(g.second)));
    assert_eq!(links_of(&f.db, g.playlist).await, vec![(g.second, 0)]);
}

/// With the files, upstream promotes first and deletes after: a primary not
/// in a mixed folder takes its whole folder — the promoted version's file
/// too — while the promoted row stays until a scan finds its file gone.
/// The merged version elsewhere keeps its file.
#[tokio::test]
async fn with_files_the_promotion_comes_first_and_the_folder_goes_after() {
    let f = fixture().await;
    let g = version_group(&f).await;

    f.manager
        .delete_item(g.primary, &with_files())
        .await
        .expect("delete");

    assert!(!g.folder.exists(), "the primary's whole folder goes");
    assert!(exists(&f, g.first).await, "the promoted row stays");
    assert!(exists(&f, g.second).await);
    assert!(f.library.join("Elsewhere/Film.mkv").is_file());
    assert_eq!(
        crate::test_support::fetch_item(&f.db, g.first)
            .await
            .primary_version_id,
        None
    );
}

/// `LibraryManager.DeleteItem` of an alternate version (`:539-552`): its
/// playlist entries name the primary instead, and the primary lists it no
/// more — a local one out of its `LocalAlternateVersions`, a merged one out
/// of its linked versions. The primary and its other versions stay.
#[tokio::test]
async fn deleting_a_version_reroutes_it_to_its_primary() {
    let f = fixture().await;
    let g = version_group(&f).await;
    let other_list = row(&f.db, BaseItemKind::Playlist, None, |_| {}).await;
    link(&f.db, other_list, g.second, 0).await;
    link(&f.db, other_list, g.merged, 0).await;

    for version in [g.second, g.merged] {
        f.manager
            .delete_item(version, &DeleteOptions::default())
            .await
            .expect("delete");
        assert!(!exists(&f, version).await);
    }

    assert_eq!(
        links_of(&f.db, other_list).await,
        vec![(g.primary, 0)],
        "both entries name the primary; a parent lists it once"
    );
    let primary = crate::test_support::fetch_item(&f.db, g.primary).await;
    assert!(
        !crate::video_versions::local_alternate_versions(primary.data.as_deref())
            .contains(&g.second_path)
    );
    assert_eq!(links_of(&f.db, g.primary).await, vec![(g.first, 2)]);
    for kept in [g.primary, g.first, g.part, g.trailer] {
        assert!(exists(&f, kept).await);
    }
}

/// A library (`CollectionFolder` row) over `locations`; with `scanned`, its
/// completed full scan by this scanner is recorded.
async fn library(f: &Fixture, locations: &[&Path], scanned: bool) -> Uuid {
    let locations: Vec<String> = locations
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let id = row(
        &f.db,
        BaseItemKind::CollectionFolder,
        Some(&f.root.path().join("server/root/default/Library")),
        |e| {
            e.data = Some(serde_json::json!({ "PhysicalLocationsList": locations }).to_string());
        },
    )
    .await;
    if scanned {
        FerrofinItemPersistenceService::new(f.db.clone())
            .record_library_scanned(id, crate::library_scan::LIBRARY_LAYOUT_GENERATION)
            .await
            .expect("record");
    }
    id
}

/// A delete with files in a library this scanner has not gone over to its
/// end yet is refused (`409`) before anything is written — no file, no
/// row, no promotion; once the library's scan is recorded, the same delete
/// goes through. An older generation's record counts as unscanned.
#[tokio::test]
async fn files_go_only_in_a_library_this_scanner_has_scanned() {
    let f = fixture().await;
    let g = version_group(&f).await;
    let id = library(&f, &[&f.library], false).await;

    let err = f
        .manager
        .delete_item(g.primary, &with_files())
        .await
        .expect_err("not scanned yet");
    assert!(matches!(err, ServiceError::Conflict(_)), "{err}");
    assert!(g.folder.is_dir(), "no file deleted");
    assert!(exists(&f, g.primary).await, "no row deleted");
    assert_eq!(
        crate::test_support::fetch_item(&f.db, g.first)
            .await
            .primary_version_id,
        Some(guid_to_db(g.primary)),
        "nothing promoted"
    );
    // Without files the rows may go: the marker guards the disk only.
    let persistence = FerrofinItemPersistenceService::new(f.db.clone());
    persistence
        .record_library_scanned(id, "0")
        .await
        .expect("record");
    assert!(matches!(
        f.manager.delete_item(g.primary, &with_files()).await,
        Err(ServiceError::Conflict(_))
    ));
    persistence
        .record_library_scanned(id, crate::library_scan::LIBRARY_LAYOUT_GENERATION)
        .await
        .expect("record");
    f.manager
        .delete_item(g.primary, &with_files())
        .await
        .expect("scanned: deleted");
    assert!(!g.folder.exists());
}

/// 1.3.x's phantom album — a `MusicAlbum` row whose path is the music
/// library's own folder — and a library location `Folder` can never be
/// deleted, scanned library or not: no file and no row goes.
#[tokio::test]
async fn a_library_folder_is_never_deleted() {
    let f = fixture().await;
    let music = f.library.join("Music");
    touch(&music.join("Artist/Album/01.flac"));
    for scanned in [false, true] {
        let library = library(&f, &[&music], scanned).await;
        let phantom = row(&f.db, BaseItemKind::MusicAlbum, Some(&music), |e| {
            e.parent_id = Some(guid_to_db(library));
        })
        .await;
        let location = row(&f.db, BaseItemKind::Folder, Some(&music), |e| {
            e.parent_id = Some(guid_to_db(library));
        })
        .await;
        for item in [phantom, location] {
            for options in [with_files(), DeleteOptions::default()] {
                let err = f
                    .manager
                    .delete_item(item, &options)
                    .await
                    .expect_err("a library's own folder");
                assert!(matches!(err, ServiceError::Unauthorized(_)), "{err}");
                assert!(exists(&f, item).await);
            }
        }
        assert!(music.join("Artist/Album/01.flac").is_file());
    }
}

/// The data directory's collections folder is never deleted; a collection
/// inside it still is.
#[tokio::test]
async fn the_collections_folder_is_never_deleted() {
    let f = fixture().await;
    let collections = f.root.path().join("server/data/collections");
    touch(&collections.join("Saga [boxset]/collection.xml"));
    let root = row(&f.db, BaseItemKind::AggregateFolder, None, |_| {}).await;
    let folder = row(&f.db, BaseItemKind::Folder, None, |e| {
        e.path = Some("%AppDataPath%/collections".to_owned());
        e.parent_id = Some(guid_to_db(root));
    })
    .await;
    let err = f
        .manager
        .delete_item(folder, &with_files())
        .await
        .expect_err("the collections folder");
    assert!(matches!(err, ServiceError::Unauthorized(_)), "{err}");
    assert!(collections.join("Saga [boxset]/collection.xml").is_file());
    assert!(exists(&f, folder).await);
}

/// A video wrongly stored as not mixed at a library's root names the
/// library's own folder: the delete is refused and the library stays.
#[tokio::test]
async fn a_delete_never_removes_a_library_folder() {
    let f = fixture().await;
    let movies = f.library.join("Movies");
    let file = movies.join("Loose.mkv");
    touch(&file);
    touch(&movies.join("Other (2001)/Other (2001).mkv"));
    library(&f, &[&movies], true).await;
    let loose = row(&f.db, BaseItemKind::Movie, Some(&file), |_| {}).await;
    assert!(
        f.manager.delete_item(loose, &with_files()).await.is_err(),
        "names the library folder"
    );
    assert!(file.is_file() && movies.join("Other (2001)/Other (2001).mkv").is_file());
    assert!(exists(&f, loose).await);
}

/// Deleting a local version with its files is upstream's `Video.
/// GetDeletePaths`: a version not in a mixed folder names its containing
/// folder — the primary's — so the whole folder goes; the primary's row
/// stays (the next scan removes it) and the version's playlist entries name
/// the primary.
#[tokio::test]
async fn deleting_a_local_version_with_files_takes_the_primarys_folder() {
    let f = fixture().await;
    let g = version_group(&f).await;
    let list = row(&f.db, BaseItemKind::Playlist, None, |_| {}).await;
    link(&f.db, list, g.second, 0).await;

    f.manager
        .delete_item(g.second, &with_files())
        .await
        .expect("delete");

    assert!(!g.folder.exists(), "the shared folder goes");
    assert!(!exists(&f, g.second).await);
    assert!(exists(&f, g.primary).await, "the primary's row stays");
    assert_eq!(links_of(&f.db, list).await, vec![(g.primary, 0)]);
}

/// Upstream promotes before it deletes files, so a first path that cannot
/// be deleted leaves a promoted group behind: the new primary stands alone
/// with the old one's versions and playlist entries, the old primary's row,
/// part and trailer stay (owned, no longer a group), and every file is
/// where it was.
#[tokio::test]
async fn a_failed_file_delete_after_a_promotion_leaves_a_consistent_group() {
    use std::os::unix::fs::PermissionsExt as _;
    if permissions_are_bypassed() {
        return;
    }
    let f = fixture().await;
    let g = version_group(&f).await;
    std::fs::set_permissions(&g.folder, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    let result = f.manager.delete_item(g.primary, &with_files()).await;
    std::fs::set_permissions(&g.folder, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        matches!(result, Err(ServiceError::Backend(_))),
        "{result:?}"
    );

    for kept in [g.primary, g.first, g.second, g.merged, g.part, g.trailer] {
        assert!(exists(&f, kept).await, "{kept} stays");
    }
    assert!(g.folder.join("Film (2000) - 1080p.mkv").is_file());
    let promoted = crate::test_support::fetch_item(&f.db, g.first).await;
    assert_eq!(promoted.primary_version_id, None);
    assert_eq!(promoted.owner_id, None);
    assert_eq!(
        links_of(&f.db, g.first).await,
        vec![(g.second, 2), (g.merged, 3)]
    );
    assert!(
        links_of(&f.db, g.primary).await.is_empty(),
        "no versions left"
    );
    assert_eq!(links_of(&f.db, g.playlist).await, vec![(g.first, 0)]);
    let old = crate::test_support::fetch_item(&f.db, g.primary).await;
    assert_eq!(old.primary_version_id, None);
    let part = crate::test_support::fetch_item(&f.db, g.part).await;
    assert_eq!(
        part.owner_id,
        Some(guid_to_db(g.primary)),
        "still the old primary's"
    );
}

/// Retention on a promotion: the deleted primary's history and the missing
/// version's go to the placeholder (detached, as any deleted row's), the
/// promoted version keeps its own.
#[tokio::test]
async fn a_promotion_detaches_the_deleted_rows_history_and_keeps_the_promoted() {
    let f = fixture().await;
    let g = version_group(&f).await;
    std::fs::remove_file(g.folder.join("Film (2000) - 4K.mkv")).expect("remove");
    let user = Uuid::new_v4();
    crate::test_support::seed_named_user(&f.db, user, "watcher").await;
    for item in [g.primary, g.first, g.second] {
        crate::test_support::seed_user_data(&f.db, user, item, true, None).await;
    }
    let placeholder = Uuid::from_u128(1);
    let before = crate::user_data_manager::user_data_row_count(&f.db, placeholder).await;

    f.manager
        .delete_item(g.primary, &DeleteOptions::default())
        .await
        .expect("delete");

    assert_eq!(
        crate::user_data_manager::user_data_row_count(&f.db, placeholder).await - before,
        2,
        "the old primary's and the missing version's history is retained"
    );
    assert_eq!(
        crate::user_data_manager::user_data_row_count(&f.db, g.second).await,
        1,
        "the promoted version keeps its own"
    );
    for gone in [g.primary, g.first] {
        assert_eq!(
            crate::user_data_manager::user_data_row_count(&f.db, gone).await,
            0
        );
    }
}
