//! A new item takes the provider ids its file or folder name carries, as
//! upstream's resolvers set them (`SetProviderIdsFromPath` in
//! `MovieResolver`, `SeriesResolver`, `SeasonResolver` and `EpisodeResolver`,
//! through `PathExtensions.GetAttributeValue`): `[tmdbid-949]`,
//! `{tmdb-8195}`, `[imdbid-tt…]`, `[tvdbid=…]`. An NFO's id still wins.

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// Creates the empty file `rel` (and its folders) under `root`.
fn touch(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dirs");
    std::fs::write(path, content).expect("fixture file");
}

/// The provider ids stored for the item whose path ends in `suffix`, as
/// `Provider=Value`.
async fn ids_of(db: &Database, suffix: &str) -> Vec<String> {
    sqlx::query_scalar(
        r#"SELECT p."ProviderId" || '=' || p."ProviderValue" FROM "BaseItemProviders" p
           JOIN "BaseItems" b ON b."Id" = p."ItemId"
           WHERE b."Path" LIKE '%' || ?1 ORDER BY 1"#,
    )
    .bind(suffix)
    .fetch_all(db.pool())
    .await
    .expect("provider ids")
}

#[tokio::test(flavor = "multi_thread")]
async fn new_items_take_the_provider_ids_in_their_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let movies = tmp.path().join("movies");
    // The folder's id; then the file's, the folder naming none; the IMDb id
    // anywhere in the path; and an NFO's id over the folder's.
    touch(&movies, "Heat (1995) [tmdbid-949]/Heat (1995).mkv", "");
    touch(
        &movies,
        "Ronin (1998)/Ronin (1998) {tmdb-8195} [imdbid-tt0122690].mkv",
        "",
    );
    touch(
        &movies,
        "Sicario (2015) [tmdbid=111]/Sicario (2015).mkv",
        "",
    );
    touch(
        &movies,
        "Sicario (2015) [tmdbid=111]/movie.nfo",
        "<movie><title>Sicario</title><tmdbid>273481</tmdbid></movie>",
    );
    let tv = tmp.path().join("tv");
    touch(
        &tv,
        "Firefly (2002) [tvdbid-78874]/Season 01 [tmdbid-3626]/Firefly S01E01 [imdbid-tt0579539].mkv",
        "",
    );

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    for (name, kind, location) in [
        ("Movies", CollectionTypeOptions::movies, &movies),
        ("Shows", CollectionTypeOptions::tvshows, &tv),
    ] {
        vf.add_virtual_folder(
            name,
            Some(kind),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: location.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
    }
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
    scanner.scan_all().await.expect("scan");

    assert_eq!(ids_of(&db, "/Heat (1995).mkv").await, ["Tmdb=949"]);
    assert_eq!(
        ids_of(&db, "[imdbid-tt0122690].mkv").await,
        ["Imdb=tt0122690", "Tmdb=8195"],
        "the file name when the folder names none, the IMDb id from the path"
    );
    assert_eq!(
        ids_of(&db, "/Sicario (2015).mkv").await,
        ["Tmdb=273481"],
        "the NFO's pinned id wins"
    );
    assert_eq!(
        ids_of(&db, "/Firefly (2002) [tvdbid-78874]").await,
        ["Tvdb=78874"]
    );
    assert_eq!(ids_of(&db, "/Season 01 [tmdbid-3626]").await, ["Tmdb=3626"]);
    assert_eq!(
        ids_of(&db, "/Firefly S01E01 [imdbid-tt0579539].mkv").await,
        ["Imdb=tt0579539"]
    );

    // Only a new item reads its name: a stored item keeps the ids it has
    // (`UpdateFromResolvedItem` carries none over).
    sqlx::query(
        r#"UPDATE "BaseItemProviders" SET "ProviderValue" = '5'
           WHERE "ProviderId" = 'Tmdb' AND "ItemId" IN
               (SELECT "Id" FROM "BaseItems" WHERE "Path" LIKE '%/Heat (1995).mkv')"#,
    )
    .execute(db.pool())
    .await
    .expect("edit the stored id");
    scanner.scan_all().await.expect("rescan");
    assert_eq!(ids_of(&db, "/Heat (1995).mkv").await, ["Tmdb=5"]);
}
