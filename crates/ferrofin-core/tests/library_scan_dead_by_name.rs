//! The by-name validators' closing sweep: after a library validation, the
//! unlocked `Genre`/`MusicGenre`, `Studio` and accessed-by-name `MusicArtist`
//! rows nothing in the library names any more are deleted
//! (`GenresValidator.cs:107-119`, `StudiosValidator.cs:108-120`,
//! `ArtistsValidator.cs:130-143`), and deleting an item drops the
//! `ItemValues` rows no item maps to any more
//! (`ItemPersistenceService.cs:149`), which is what lets a genre die.

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;

/// A movie folder whose Kodi sidecar names one genre and one studio.
fn write_movie(media: &Path, folder: &str, genre: &str, studio: &str) {
    let dir = media.join(folder);
    std::fs::create_dir_all(&dir).expect("fixture dirs");
    std::fs::write(dir.join(format!("{folder}.mkv")), b"").expect("media file");
    std::fs::write(
        dir.join("movie.nfo"),
        format!(
            r#"<?xml version="1.0"?>
<movie><title>{folder}</title><genre>{genre}</genre><studio>{studio}</studio></movie>"#
        ),
    )
    .expect("nfo");
}

/// The names of the rows of `kind`.
async fn names(items: &Arc<dyn ItemRepository>, kind: BaseItemKind) -> Vec<String> {
    let mut names: Vec<String> = items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![kind],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("query")
        .into_iter()
        .filter_map(|row| row.name)
        .collect();
    names.sort();
    names
}

/// Inserts a by-name row of `type_name` (parentless, as the item-values step
/// materializes them), locked or not.
async fn seed_by_name(db: &Database, id: &str, type_name: &str, name: &str, locked: bool) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "CleanName")
           VALUES (?1, ?2, 1, 0, ?4, 0, 0, 0, 0, ?3, lower(?3))"#,
    )
    .bind(id)
    .bind(type_name)
    .bind(name)
    .bind(locked)
    .execute(db.pool())
    .await
    .expect("seed by-name row");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_validation_deletes_the_by_name_rows_nothing_names_any_more() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    write_movie(&media, "Heat (1995)", "Crime", "Warner Bros.");
    write_movie(&media, "Ronin (1998)", "Action", "United Artists");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
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
    .with_items(Arc::clone(&items));

    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        names(&items, BaseItemKind::Genre).await,
        ["Action", "Crime"]
    );
    assert_eq!(
        names(&items, BaseItemKind::Studio).await,
        ["United Artists", "Warner Bros."]
    );

    // A by-name artist no track credits, and a locked genre nothing names.
    let artist = ferrofin_core::item_type_lookup::stored_type_name(BaseItemKind::MusicArtist)
        .expect("artist type");
    let genre =
        ferrofin_core::item_type_lookup::stored_type_name(BaseItemKind::Genre).expect("genre type");
    seed_by_name(
        &db,
        "0000000A-0000-0000-0000-00000000000A",
        artist,
        "Ghost",
        false,
    )
    .await;
    seed_by_name(
        &db,
        "0000000B-0000-0000-0000-00000000000B",
        genre,
        "Pinned",
        true,
    )
    .await;

    // The only movie naming Action and United Artists leaves the library.
    std::fs::remove_dir_all(media.join("Ronin (1998)")).expect("remove movie");
    scanner.scan_all().await.expect("second scan");

    assert_eq!(
        names(&items, BaseItemKind::Genre).await,
        ["Crime", "Pinned"],
        "the dead genre goes, the locked one stays"
    );
    assert_eq!(names(&items, BaseItemKind::Studio).await, ["Warner Bros."]);
    assert!(
        names(&items, BaseItemKind::MusicArtist).await.is_empty(),
        "the by-name artist nothing credits goes"
    );
}
