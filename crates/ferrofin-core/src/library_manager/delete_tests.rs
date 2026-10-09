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
use ferrofin_traits::persistence::ItemPersistenceService;
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
/// containing folder it was given.
#[derive(Default)]
struct RecordingExternalData {
    deleted: Mutex<Vec<(Uuid, String, String)>>,
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
    .with_app_paths(paths);
    let external = Arc::new(RecordingExternalData::default());
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
/// and, after the rows, its extracted data (`DeleteExternalItemFiles`) with
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
        vec![
            // `GetInternalMetadataPaths` of a video, before the files.
            (alien, String::new(), String::new()),
            // `DeleteExternalItemFiles`, after the rows.
            (alien, path, folder),
        ]
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

    f.manager
        .delete_item(movie, &with_files())
        .await
        .expect("delete");

    assert!(!folder.exists(), "the movie's whole folder is deleted");
    assert!(!exists(&f, movie).await);
    assert!(!exists(&f, trailer).await, "the owned extra's row goes too");
    assert!(!metadata.exists(), "the internal metadata folder goes too");
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
    // The children's extracted data is cleaned after the rows, the season
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
