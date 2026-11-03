//! gl-941: the box sets and playlists the server makes in its data directory
//! store their folder as `%AppDataPath%/…`, as upstream's save does
//! (`BaseItemRepository` → `BaseItemMapper.Map` → `GetPathToSave` →
//! `ReverseVirtualPath`), and read back expanded under whichever data
//! directory the server has. A moved data directory then needs no repair for
//! them, and a save of the row read back keeps the token.

use std::path::Path;
use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_model::playlists::PlaylistCreationRequest;
use ferrofin_traits::collections::{CollectionCreationOptions, CollectionManager, PlaylistManager};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::system::ServerApplicationPaths;
use uuid::Uuid;

use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::ItemTypeLookup;
use crate::linked_children_service::FerrofinLinkedChildrenService;
use crate::test_support::{fetch_item, item_repository_over, library_manager_over, test_db};
use crate::virtual_paths::VirtualPathExpander;

use super::tests::provision_root_folder;
use super::{FerrofinCollectionManager, FerrofinPlaylistManager};

/// A server's directories under `program_data`, and the expander that follows
/// them.
fn directories(program_data: &Path) -> (Arc<dyn ServerApplicationPaths>, VirtualPathExpander) {
    let paths = Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
        program_data,
        program_data.join("log"),
        program_data.join("config"),
        program_data.join("cache"),
        program_data.join("web"),
    ));
    let expander = VirtualPathExpander::from_paths(Arc::clone(&paths));
    (paths, expander)
}

/// The row `id` as a server whose directories `expander` follows reads it.
async fn read_row(
    db: &Database,
    expander: &VirtualPathExpander,
    id: Uuid,
) -> ferrofin_db::entities::base_items::BaseItemEntity {
    FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
        .with_virtual_paths(expander.clone())
        .retrieve_item(id)
        .await
        .expect("read")
        .expect("the row")
}

/// The `Path` of `id` as a server whose directories `expander` follows reads it.
async fn read(db: &Database, expander: &VirtualPathExpander, id: Uuid) -> String {
    read_row(db, expander, id).await.path.expect("a path")
}

/// The `Path` column of `id` as stored.
async fn stored(db: &Database, id: Uuid) -> Option<String> {
    fetch_item(db, id).await.path
}

/// Reads `id` under the data directory `moved` follows, checks it expands to
/// `expected`, and saves the row back as a refresh does: the stored form
/// stays `token`.
async fn read_after_a_move_and_save(
    db: &Database,
    moved: &VirtualPathExpander,
    id: Uuid,
    expected: &str,
    token: &str,
) {
    let mut row = read_row(db, moved, id).await;
    assert_eq!(
        row.path.as_deref(),
        Some(expected),
        "read under the moved data directory"
    );
    row.overview = Some("refreshed".to_owned());
    FerrofinItemPersistenceService::new(db.clone())
        .with_virtual_paths(moved.clone())
        .save_items(std::slice::from_ref(&row))
        .await
        .expect("save");
    assert_eq!(
        stored(db, id).await.as_deref(),
        Some(token),
        "a save keeps it"
    );
}

/// `CreateCollectionAsync` saves the box set through `AddChild`, whose save
/// stores `%AppDataPath%/collections/<name> [boxset]`; the folder itself is
/// made at the real path.
#[tokio::test]
async fn a_created_box_set_stores_its_folder_under_the_data_path_token() {
    let db = test_db().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (paths, expander) = directories(&tmp.path().join("old"));
    let mgr = FerrofinCollectionManager::new(
        db.clone(),
        library_manager_over(db.clone()),
        Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
        Arc::clone(&paths),
    )
    .with_virtual_paths(expander.clone());

    let created = mgr
        .create_collection(&CollectionCreationOptions {
            name: "Heist".to_owned(),
            ..CollectionCreationOptions::default()
        })
        .await
        .expect("create");
    let id = Uuid::parse_str(&created.id).expect("id");

    let token = "%AppDataPath%/collections/Heist [boxset]";
    assert_eq!(stored(&db, id).await.as_deref(), Some(token));
    let folder = format!("{}/collections/Heist [boxset]", paths.data_path());
    assert!(Path::new(&folder).is_dir(), "{folder} is made");
    assert_eq!(read(&db, &expander, id).await, folder);

    let (moved_paths, moved) = directories(&tmp.path().join("new"));
    let expected = format!("{}/collections/Heist [boxset]", moved_paths.data_path());
    read_after_a_move_and_save(&db, &moved, id, &expected, token).await;
}

/// `PlaylistManager.CreatePlaylist` saves the playlist through `AddChild` as
/// well: `%AppDataPath%/playlists/<name>`.
#[tokio::test]
async fn a_created_playlist_stores_its_folder_under_the_data_path_token() {
    let db = test_db().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (paths, expander) = directories(&tmp.path().join("old"));
    let mgr = FerrofinPlaylistManager::new(
        db.clone(),
        library_manager_over(db.clone()),
        item_repository_over(db.clone()),
        Arc::clone(&paths),
    )
    .with_virtual_paths(expander.clone());
    provision_root_folder(&db, &paths).await;

    let created = mgr
        .create_playlist(&PlaylistCreationRequest {
            name: Some("Road Trip".to_owned()),
            user_id: Uuid::new_v4(),
            ..PlaylistCreationRequest::default()
        })
        .await
        .expect("create");
    let id = Uuid::parse_str(&created.id).expect("id");

    let token = "%AppDataPath%/playlists/Road Trip";
    assert_eq!(stored(&db, id).await.as_deref(), Some(token));
    let folder = format!("{}/playlists/Road Trip", paths.data_path());
    assert!(Path::new(&folder).is_dir(), "{folder} is made");
    assert_eq!(read(&db, &expander, id).await, folder);

    let (moved_paths, moved) = directories(&tmp.path().join("new"));
    let expected = format!("{}/playlists/Road Trip", moved_paths.data_path());
    read_after_a_move_and_save(&db, &moved, id, &expected, token).await;
}
