//! Identify → Apply on a book (`POST /Items/RemoteSearch/Apply`): the refresh
//! carries the chosen search result, and upstream runs none of the item's
//! local metadata providers then ("Do not execute local providers if we are
//! identifying", `MetadataService.RefreshWithProviders`). A book's embedded
//! readers (`ComicProvider`, which runs `ComicBookInfoProvider`, `EpubProvider`
//! and `OpfProvider`) are such providers, so the file's values do not refill
//! the item: only the chosen provider answers.
//!
//! The chosen result's provider runs first, whichever provider it is: a
//! plugin's named metadata source too (`RefreshWithProviders` orders every
//! remote provider by `SearchProviderName`), for a movie as for a book.

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner, ScanCancel, ScanPasses, ScanRun,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::providers::RemoteSearchResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::providers::{
    DynamicMetadataLookup, DynamicMetadataProvider, DynamicMetadataResult, MetadataRefreshMode,
    MetadataRefreshOptions,
};

/// A plugin metadata source that answers every book with one genre.
struct ChosenDb;

#[async_trait::async_trait]
impl DynamicMetadataProvider for ChosenDb {
    fn name(&self) -> &'static str {
        "chosen-db"
    }

    async fn lookup(
        &self,
        item: &DynamicMetadataLookup,
    ) -> Result<Option<DynamicMetadataResult>, ServiceError> {
        Ok((item.kind == "Book").then(|| DynamicMetadataResult {
            genres: vec!["Chosen genre".to_owned()],
            ..DynamicMetadataResult::default()
        }))
    }
}

/// A books library over `media`, scanned with [`ChosenDb`].
async fn books_library(tmp: &Path, media: &Path) -> (Database, LibraryScanner) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Books",
        Some(CollectionTypeOptions::books),
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
    .with_items(items)
    .with_dynamic_providers(vec![Arc::new(ChosenDb)]);
    (db, scanner)
}

#[tokio::test(flavor = "multi_thread")]
async fn identifying_a_book_does_not_read_its_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("books");
    std::fs::create_dir_all(&media).expect("media");
    // Not a ZIP: only the `<stem>.opf` sidecar reads.
    let book = media.join("Dune.epub");
    std::fs::write(&book, b"not an archive").expect("book");
    std::fs::write(
        media.join("Dune.opf"),
        r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>Dune</dc:title><dc:description>Desert planet.</dc:description><dc:subject>Sci-Fi</dc:subject></metadata></package>"#,
    )
    .expect("opf");
    let (db, scanner) = books_library(tmp.path(), &media).await;
    let path = book.to_string_lossy().into_owned();
    let id = derive_item_id(BaseItemKind::Book, &path).expect("id");
    let items = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));
    let stored = || async {
        let row = items.retrieve_item(id).await.expect("read").expect("book");
        (row.name, row.overview, row.genres)
    };

    scanner.scan_all().await.expect("scan");
    // The OPF is the first source; the plugin fills only what it left.
    assert_eq!(
        stored().await,
        (
            Some("Dune".into()),
            Some("Desert planet.".into()),
            Some("Sci-Fi".into())
        )
    );

    // The user clears the overview, then identifies the book.
    let cleared = sqlx::query(r#"UPDATE "BaseItems" SET "Overview" = NULL WHERE "Path" = ?1"#)
        .bind(&path)
        .execute(db.pool())
        .await
        .expect("clear overview");
    assert_eq!(cleared.rows_affected(), 1);
    let chosen = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        remove_old_metadata: true,
        search_result: Some(RemoteSearchResult {
            name: Some("Dune".into()),
            search_provider_name: Some("chosen-db".into()),
            ..RemoteSearchResult::default()
        }),
        ..MetadataRefreshOptions::default()
    };
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    scanner
        .scan_target(
            &ScanTarget::Items(vec![path.clone()]),
            ScanRun::new(&chosen, &none, &ScanCancel::new()).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("identify");
    assert_eq!(
        stored().await,
        (Some("Dune".into()), None, Some("Chosen genre".into())),
        "the OPF does not refill the cleared overview, and the chosen \
         provider's genre replaces the OPF's"
    );
}

/// A named (library-gated) plugin metadata source that answers every book
/// and movie with its own genre.
struct NamedSource(&'static str);

#[async_trait::async_trait]
impl DynamicMetadataProvider for NamedSource {
    fn name(&self) -> &str {
        self.0
    }

    fn library_gated(&self) -> bool {
        true
    }

    async fn lookup(
        &self,
        item: &DynamicMetadataLookup,
    ) -> Result<Option<DynamicMetadataResult>, ServiceError> {
        Ok(
            matches!(item.kind.as_str(), "Book" | "Movie").then(|| DynamicMetadataResult {
                genres: vec![self.0.to_owned()],
                ..DynamicMetadataResult::default()
            }),
        )
    }
}

/// Identifying runs the provider the chosen result came from first ("When
/// identifying, run the provider the user picked first so the correct IDs
/// are used", `MetadataService.cs:876-882`), comparing every remote
/// provider's `Name`: a plugin's named source too, not only TheTVDB,
/// TheMovieDb and OMDb. Unranked, the two sources run in registration
/// order, so the first one's genre wins a plain scan; the Identify that
/// names the second runs it first, and its genre wins.
#[rstest::rstest]
#[case::book(CollectionTypeOptions::books, "Dune.epub", BaseItemKind::Book)]
#[case::movie(CollectionTypeOptions::movies, "Heat (1995).mkv", BaseItemKind::Movie)]
#[tokio::test(flavor = "multi_thread")]
async fn identifying_runs_the_chosen_plugin_source_first(
    #[case] collection: CollectionTypeOptions,
    #[case] file: &str,
    #[case] kind: BaseItemKind,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("media");
    std::fs::create_dir_all(&media).expect("media");
    let path = media.join(file);
    std::fs::write(&path, b"not media").expect("file");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Media",
        Some(collection),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    let items = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items.clone())
    .with_dynamic_providers(vec![
        Arc::new(NamedSource("first-db")),
        Arc::new(NamedSource("chosen-db")),
    ]);
    let path = path.to_string_lossy().into_owned();
    let id = derive_item_id(kind, &path).expect("id");
    let genres = || async {
        items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("item")
            .genres
    };

    scanner.scan_all().await.expect("scan");
    assert_eq!(genres().await.as_deref(), Some("first-db"));

    let chosen = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        remove_old_metadata: true,
        search_result: Some(RemoteSearchResult {
            name: Some("Chosen".into()),
            search_provider_name: Some("Chosen-DB".into()),
            ..RemoteSearchResult::default()
        }),
        ..MetadataRefreshOptions::default()
    };
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    scanner
        .scan_target(
            &ScanTarget::Items(vec![path.clone()]),
            ScanRun::new(&chosen, &none, &ScanCancel::new()).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("identify");
    assert_eq!(
        genres().await.as_deref(),
        Some("chosen-db"),
        "the chosen source answered first, so its genre wins"
    );
}
