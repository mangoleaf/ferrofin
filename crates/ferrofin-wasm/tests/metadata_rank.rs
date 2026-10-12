//! A WASM plugin's metadata source runs at its configured rank among a
//! library's remote providers, end to end: a real `ferrofin:plugin`
//! component (an **inline WAT fixture**, compiled at test time — no `.wasm`
//! in the repo) loaded by the plugin host, its `metadata-lookup` export
//! folded by a real library scan with TheMovieDb, exactly as upstream's
//! `MetadataService.ExecuteRemoteProviders` folds every remote provider in
//! `ProviderManager.GetMetadataProvidersInternal`'s order (the library's
//! `MetadataFetcherOrder`, then `IHasOrder`, which a WASM plugin cannot
//! declare: upstream's 50).
//!
//! The plugin (`WatDb`, a named provider for movies) answers every movie
//! with an overview, a community rating, a tagline and its own id; the
//! TheMovieDb stand-in answers The Matrix with an overview, a rating and a
//! release date, but no tagline. So which provider won a field both answer
//! is visible on the saved row, and so is a gap only the plugin fills.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, TypeOptions};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_providers::TmdbClient;
use ferrofin_providers::library_options::fetcher_names::TMDB;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use ferrofin_wasm::{WasmPluginHost, WasmSettings};
use uuid::Uuid;

mod common;

use common::{
    EnabledStub, FIXTURE_OVERVIEW, FIXTURE_RATING, FIXTURE_TAGLINE, LookupAnswer,
    metadata_provider_fixture,
};

/// The plugin's advertised provider name — what the library-options fetcher
/// lists show and `MetadataFetcherOrder` stores.
const WATDB: &str = "WatDb";

/// TheMovieDb's overview of The Matrix.
const TMDB_OVERVIEW: &str = "About The Matrix.";

/// TheMovieDb's community rating of The Matrix.
const TMDB_RATING: f64 = 8.0;

/// A TheMovieDb stand-in for one movie (The Matrix → 603) that can be
/// switched to answer "nothing here" (a 404: a miss, not a failure).
struct Tmdb {
    base: String,
    misses: Arc<AtomicBool>,
}

impl Tmdb {
    fn spawn() -> Self {
        let misses = Arc::new(AtomicBool::new(false));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let missing = Arc::clone(&misses);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = request.lines().next().unwrap_or_default().to_owned();
                let body = if missing.load(Ordering::SeqCst) {
                    None
                } else if line.contains("/search/movie") {
                    Some(r#"{"results": [{"id": 603, "title": "The Matrix"}]}"#.to_owned())
                } else if line.contains("/movie/603?") {
                    // A trailer, so no backfill asks again on its own.
                    Some(format!(
                        r#"{{"id": 603, "title": "The Matrix", "overview": "{TMDB_OVERVIEW}",
                            "vote_average": {TMDB_RATING}, "release_date": "1999-03-30",
                            "videos": {{"results": [{{"site": "YouTube", "type": "Trailer",
                                "key": "k", "name": "Trailer"}}]}}}}"#
                    ))
                } else {
                    None
                };
                let (status, payload) =
                    body.map_or(("404 Not Found", "{}".to_owned()), |b| ("200 OK", b));
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        Self {
            base: format!("http://{addr}"),
            misses,
        }
    }

    /// From now on, TheMovieDb has nothing for anything.
    fn miss_everything(&self) {
        self.misses.store(true, Ordering::SeqCst);
    }
}

/// Loads the one-plugin directory `plugins` and arms the host, as the
/// composition root does once the server's managers exist (an unarmed host's
/// sources are inert). Every plugin is enabled.
async fn load_host(plugins: &Path) -> WasmPluginHost {
    let dir = plugins.to_path_buf();
    let host =
        tokio::task::spawn_blocking(move || WasmPluginHost::load(&dir, &WasmSettings::default()))
            .await
            .expect("load task")
            .expect("load the plugins");
    assert_eq!(host.plugins().len(), 1, "the fixture plugin loads");
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        lyrics: Arc::new(common::StubLyrics::default()),
        subtitles: Arc::new(common::StubSubtitles::default()),
        collections: Arc::new(common::StubCollections::default()),
        media_streams: Arc::new(common::StubStreams),
        extractor: Arc::new(common::StubExtractor::default()),
        analysis: Arc::new(tokio::sync::Semaphore::new(1)),
        users: Arc::new(common::StubUsers),
        user_data: Arc::new(common::StubUserData),
        tv: Arc::new(common::StubTv),
        handle: tokio::runtime::Handle::current(),
        library: Arc::new(common::OneMovieLibrary {
            seen: std::sync::Mutex::new(None),
        }),
        media_segments: Arc::new(common::RecordingSegments::default()),
        plugins: Arc::new(EnabledStub(b"{}".to_vec())),
    });
    host
}

/// A movie library holding `The Matrix (1999)/The Matrix (1999).mkv` (and,
/// with `nfo`, a `movie.nfo` naming only its title), scanned with
/// TheMovieDb and the WASM plugin answering `answer`.
struct Library {
    _tmp: tempfile::TempDir,
    _host: WasmPluginHost,
    tmdb: Tmdb,
    scanner: LibraryScanner,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: Arc<dyn ItemRepository>,
    movie: PathBuf,
}

impl Library {
    async fn new(options: LibraryOptions, answer: LookupAnswer, nfo: bool) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).expect("plugins dir");
        let wat = metadata_provider_fixture("77777777-7777-7777-7777-777777777777", WATDB, answer);
        let component = wat::parse_str(&wat).expect("the fixture WAT compiles");
        std::fs::write(plugins.join("watdb.wasm"), component).expect("write the component");
        let host = load_host(&plugins).await;

        let media = tmp.path().join("movies");
        let folder = media.join("The Matrix (1999)");
        std::fs::create_dir_all(&folder).expect("mkdir");
        let movie = folder.join("The Matrix (1999).mkv");
        std::fs::write(&movie, b"0123456789").expect("write the movie");
        if nfo {
            std::fs::write(
                folder.join("movie.nfo"),
                b"<movie><title>The Matrix</title></movie>",
            )
            .expect("write the nfo");
        }
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
                ..options
            },
        )
        .await
        .expect("add the library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let tmdb = Tmdb::spawn();
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_items(Arc::clone(&items))
                .with_metadata(
                    Arc::new(TmdbClient::new().with_base_url(&tmdb.base)),
                    tmp.path().join("metadata"),
                )
                .with_dynamic_providers(host.metadata_providers())
                .with_progress_every(0);
        Self {
            _tmp: tmp,
            _host: host,
            tmdb,
            scanner,
            persistence,
            items,
            movie,
        }
    }

    fn movie_id(&self) -> Uuid {
        derive_item_id(BaseItemKind::Movie, &self.movie.to_string_lossy()).expect("id")
    }

    async fn movie_row(&self) -> BaseItemEntity {
        self.items
            .retrieve_item(self.movie_id())
            .await
            .expect("read")
            .expect("the movie")
    }

    async fn movie_ids(&self) -> Vec<(String, String)> {
        let id = self.movie_id();
        let mut ids = self
            .persistence
            .provider_ids_for_items(&[id])
            .await
            .expect("ids")
            .remove(&id)
            .unwrap_or_default();
        ids.sort();
        ids
    }
}

/// A library whose admin ticked TheMovieDb and the plugin for movies and
/// saved `order` as their order.
fn movie_order(order: &[&str]) -> LibraryOptions {
    LibraryOptions {
        type_options: vec![TypeOptions {
            type_: Some("Movie".to_owned()),
            metadata_fetchers: vec![TMDB.to_owned(), WATDB.to_owned()],
            metadata_fetcher_order: order.iter().map(|n| (*n).to_owned()).collect(),
            ..TypeOptions::default()
        }],
        ..LibraryOptions::default()
    }
}

/// The dashboard's three refresh choices.
fn dashboard(choice: &str) -> MetadataRefreshOptions {
    use MetadataRefreshMode::{Default, FullRefresh};
    match choice {
        "scan" => MetadataRefreshOptions::for_item_refresh(Default, Default, false, false, false),
        "missing" => {
            MetadataRefreshOptions::for_item_refresh(FullRefresh, FullRefresh, false, false, false)
        }
        "replace" => {
            MetadataRefreshOptions::for_item_refresh(FullRefresh, FullRefresh, true, false, false)
        }
        other => panic!("no choice {other}"),
    }
}

/// Ranked before TheMovieDb, the plugin's answer wins every field it fills —
/// the overview and rating TheMovieDb answers too — and TheMovieDb, folded
/// after it, fills only what it left: the premiere date and year. Both
/// providers' ids are recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_wasm_source_ranked_before_tmdb_wins_a_field_tmdb_also_answers() {
    let library = Library::new(movie_order(&[WATDB, TMDB]), LookupAnswer::Metadata, false).await;
    library.scanner.scan_all().await.expect("scan");
    let movie = library.movie_row().await;
    assert_eq!(movie.overview.as_deref(), Some(FIXTURE_OVERVIEW));
    assert_eq!(movie.community_rating, Some(FIXTURE_RATING));
    assert_eq!(movie.tagline.as_deref(), Some(FIXTURE_TAGLINE));
    assert_eq!(
        movie.production_year,
        Some(1999),
        "TheMovieDb fills the year"
    );
    assert!(
        movie.date_last_refreshed.is_some(),
        "both answered: the refresh is complete"
    );
    assert_eq!(
        library.movie_ids().await,
        [
            ("Tmdb".to_owned(), "603".to_owned()),
            ("WatDb".to_owned(), "w1".to_owned())
        ]
    );
}

/// Ranked after TheMovieDb, the plugin only fills what TheMovieDb left: its
/// tagline lands, TheMovieDb's overview and rating stand.
#[tokio::test(flavor = "multi_thread")]
async fn a_wasm_source_ranked_after_tmdb_only_fills_the_gaps() {
    let library = Library::new(movie_order(&[TMDB, WATDB]), LookupAnswer::Metadata, false).await;
    library.scanner.scan_all().await.expect("scan");
    let movie = library.movie_row().await;
    assert_eq!(movie.overview.as_deref(), Some(TMDB_OVERVIEW));
    assert_eq!(movie.community_rating, Some(TMDB_RATING));
    assert_eq!(movie.tagline.as_deref(), Some(FIXTURE_TAGLINE));
    assert!(
        library
            .movie_ids()
            .await
            .contains(&("WatDb".to_owned(), "w1".to_owned())),
        "a provider ranked after still records its own id"
    );
}

/// A plugin the saved order leaves out — a library that never saved movie
/// options, one whose order is empty, or one ranking only TheMovieDb — ranks
/// by its `IHasOrder`, upstream's 50 for a provider that declares none
/// (`GetDefaultOrder`), so TheMovieDb (`IHasOrder` 1) runs before it and it
/// fills only what TheMovieDb left.
#[rstest::rstest]
#[case::no_saved_options(LibraryOptions::default())]
#[case::empty_order(movie_order(&[]))]
#[case::only_tmdb_ranked(movie_order(&[TMDB]))]
#[tokio::test(flavor = "multi_thread")]
async fn an_unranked_wasm_source_takes_the_default_order(#[case] options: LibraryOptions) {
    let library = Library::new(options, LookupAnswer::Metadata, false).await;
    library.scanner.scan_all().await.expect("scan");
    let movie = library.movie_row().await;
    assert_eq!(movie.overview.as_deref(), Some(TMDB_OVERVIEW));
    assert_eq!(movie.community_rating, Some(TMDB_RATING));
    assert_eq!(movie.tagline.as_deref(), Some(FIXTURE_TAGLINE));
}

/// A plugin whose `metadata-lookup` errors is a provider that FAILED, like a
/// built-in one whose request fails (`ExecuteRemoteProviders` counts it,
/// `MetadataService.cs:1015-1022`): ranked first, it keeps a scan in which
/// TheMovieDb answered from being recorded as a completed refresh, so every
/// later scan asks again. And it clears nothing: once TheMovieDb has nothing
/// either, a refresh in any mode keeps the stored overview and rating —
/// "Replace all metadata" included, which erases old values only when a
/// remote provider answered with something to replace them
/// (`MetadataService.cs:899-906`; the movie's NFO is what this pass read).
#[rstest::rstest]
#[case::scan("scan")]
#[case::search_missing("missing")]
#[case::replace_all("replace")]
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_wasm_source_is_a_failure_that_clears_nothing(#[case] choice: &str) {
    let library = Library::new(movie_order(&[WATDB, TMDB]), LookupAnswer::Down, true).await;
    library.scanner.scan_all().await.expect("scan");
    let movie = library.movie_row().await;
    assert_eq!(
        movie.overview.as_deref(),
        Some(TMDB_OVERVIEW),
        "TheMovieDb still answers"
    );
    assert_eq!(movie.community_rating, Some(TMDB_RATING));
    assert!(
        movie.date_last_refreshed.is_none(),
        "the plugin's failure keeps the refresh from being stamped"
    );

    library.tmdb.miss_everything();
    library
        .scanner
        .scan_with(None, &dashboard(choice))
        .await
        .expect("rescan");
    let movie = library.movie_row().await;
    assert_eq!(
        movie.name.as_deref(),
        Some("The Matrix"),
        "the NFO was read"
    );
    assert_eq!(
        movie.overview.as_deref(),
        Some(TMDB_OVERVIEW),
        "{choice}: a failed provider erases nothing"
    );
    assert_eq!(movie.community_rating, Some(TMDB_RATING), "{choice}");
    assert!(
        movie.date_last_refreshed.is_none(),
        "{choice}: still unstamped, so the next scan asks again"
    );
}
