//! A scan reads an NFO sidecar whatever its text encoding, as upstream's
//! `BaseNfoParser.Fetch` does through `new StreamReader(fs, Encoding.UTF8)`:
//! a UTF-16 BOM picks UTF-16, and a byte that is not UTF-8 (a Latin-1 title)
//! becomes U+FFFD instead of dropping the whole document.

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

/// A one-movie library whose NFO holds `nfo`, and a scanner over it.
async fn movie_with_nfo(root: &Path, nfo: &[u8]) -> (LibraryScanner, Database) {
    let media = root.join("movies");
    let folder = media.join("Movie (2020)");
    std::fs::create_dir_all(&folder).expect("fixture dirs");
    std::fs::write(folder.join("Movie (2020).mkv"), b"").expect("media file");
    std::fs::write(folder.join("Movie (2020).nfo"), nfo).expect("nfo");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
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
    .with_items(items);
    (scanner, db)
}

/// `text` as UTF-16 with a byte-order mark, little- or big-endian.
fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
    let bom: &[u8] = if little_endian {
        &[0xFF, 0xFE]
    } else {
        &[0xFE, 0xFF]
    };
    bom.iter()
        .copied()
        .chain(text.encode_utf16().flat_map(|unit| {
            if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            }
        }))
        .collect()
}

const NFO: &str = "<movie><title>Café</title><tmdbid>603</tmdbid></movie>";

#[rstest::rstest]
#[case::utf16_le(utf16(NFO, true), "Café")]
#[case::utf16_be(utf16(NFO, false), "Café")]
#[case::utf8_bom([&[0xEF, 0xBB, 0xBF][..], NFO.as_bytes()].concat(), "Café")]
#[case::latin1(
    b"<movie><title>Caf\xE9</title><tmdbid>603</tmdbid></movie>".to_vec(),
    "Caf\u{FFFD}"
)]
#[tokio::test]
async fn an_nfo_that_is_not_plain_utf8_is_still_read(#[case] nfo: Vec<u8>, #[case] title: &str) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = movie_with_nfo(tmp.path(), &nfo).await;
    scanner.scan_all().await.expect("scan");

    let (name, tmdb): (String, String) = sqlx::query_as(
        r#"SELECT b."Name", p."ProviderValue" FROM "BaseItems" b
           JOIN "BaseItemProviders" p ON p."ItemId" = b."Id" AND p."ProviderId" = 'Tmdb'
           WHERE b."Type" LIKE '%.Movie'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("the NFO's title and pinned id were stored");
    assert_eq!((name.as_str(), tmdb.as_str()), (title, "603"));
}
