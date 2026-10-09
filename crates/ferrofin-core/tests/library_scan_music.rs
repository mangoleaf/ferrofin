//! The music pass and the closing passes under the per-item refresh decision
//! (`PLAN_SCAN_CHANGE_DETECTION` Phase 6), end to end through a real scan.
//!
//! Over the real scanner, repositories and SQLite schema, with a MusicBrainz
//! and TheAudioDB stand-in recording every request:
//!
//! - a first scan asks the music providers once per album and artist, an
//!   unchanged rescan asks nothing, a new album asks for itself only;
//! - a stored match is never searched again, "Replace all metadata" asks
//!   again by it;
//! - a library's unticked checkboxes ask nothing, a locked album or artist
//!   neither;
//! - a provider that fails leaves the item unstamped, so the next scan asks
//!   again (owner decision D1);
//! - a by-name artist's refresh, and an Identify of an album or artist, go
//!   through the scanner and fetch by the chosen ids;
//! - a series' `DateLastMediaAdded` follows its episodes, and the
//!   "date added" folder sort with it;
//! - the library-wide closing passes run on every validation, each by its
//!   own persisted selection, so what a cancelled scan, a failed lookup or a
//!   write outside the scanner left undone is done by the next one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner, ScanCancel, ScanOutcome, ScanPasses, ScanRun,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, TypeOptions};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::providers::RemoteSearchResult;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::providers::{
    DynamicMetadataLookup, DynamicMetadataProvider, DynamicMetadataResult, MetadataRefreshMode,
    MetadataRefreshOptions,
};
use uuid::Uuid;

/// The ids every stand-in MusicBrainz answer names.
const RELEASE: &str = "11111111-1111-4111-8111-111111111111";
const GROUP: &str = "22222222-2222-4222-8222-222222222222";
const ARTIST: &str = "33333333-3333-4333-8333-333333333333";
/// The ids an Identify picks.
const CHOSEN_RELEASE: &str = "44444444-4444-4444-8444-444444444444";
const CHOSEN_GROUP: &str = "55555555-5555-4555-8555-555555555555";
const CHOSEN_ARTIST: &str = "66666666-6666-4666-8666-666666666666";

/// A MusicBrainz + TheAudioDB (and studio artwork) stand-in on one port,
/// recording each request line, that refuses the requests
/// [`fail`](Self::fail) names with a `400`: a failed request (owner decision
/// D1), and one the clients do not retry or back off from, so the next
/// answer is not delayed.
struct Music {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    failing: Arc<Mutex<Option<&'static str>>>,
}

impl Music {
    fn spawn() -> Self {
        use std::io::{Read as _, Write as _};
        let requests = Arc::new(Mutex::new(Vec::new()));
        let failing: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let log = Arc::clone(&requests);
        let fails = Arc::clone(&failing);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                log.lock().expect("lock").push(line.clone());
                if fails
                    .lock()
                    .expect("lock")
                    .is_some_and(|path| line.contains(path))
                {
                    let _ = write!(
                        s,
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    continue;
                }
                let release = format!(
                    r#"{{"id":"{RELEASE}","title":"Kind of Blue","date":"1997-03-25","release-group":{{"id":"{GROUP}"}},"artist-credit":[{{"name":"Miles Davis"}}],"label-info":[{{"label":{{"name":"Columbia"}}}}]}}"#
                );
                let body = if line.contains("/ws/2/release-group/") {
                    format!(
                        r#"{{"title":"Kind of Blue","first-release-date":"1959-08-17","releases":[{{"id":"{RELEASE}"}}],"artist-credit":[{{"name":"Miles Davis"}}],"genres":[{{"name":"modal jazz","count":3}},{{"name":"jazz","count":10}}],"tags":[{{"name":"trumpet","count":2}}]}}"#
                    )
                } else if line.contains("/ws/2/release/") {
                    release
                } else if line.contains("/ws/2/release?") {
                    format!(r#"{{"releases":[{release}]}}"#)
                } else if line.contains("/ws/2/artist/") {
                    format!(
                        r#"{{"id":"{ARTIST}","name":"Miles Davis","life-span":{{"begin":"1926-05-26","end":"1991-09-28"}},"area":{{"name":"United States"}},"genres":[{{"name":"jazz","count":5}}],"tags":[{{"name":"trumpeter","count":1}}]}}"#
                    )
                } else if line.contains("/ws/2/artist?") {
                    format!(r#"{{"artists":[{{"id":"{ARTIST}","name":"Miles Davis"}}]}}"#)
                } else if line.contains("/album-mb.php") {
                    r#"{"album":[{"strDescriptionEN":"Album text.","strGenre":"Jazz"}]}"#.to_owned()
                } else {
                    r#"{"artists":[{"strBiographyEN":"Artist bio.","strGenre":"Jazz"}]}"#.to_owned()
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            base: format!("http://{addr}"),
            requests,
            failing,
        }
    }

    /// Refuses every request whose line contains `path` from now on (`None`:
    /// none).
    fn fail(&self, path: Option<&'static str>) {
        *self.failing.lock().expect("lock") = path;
    }

    /// The request lines since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().expect("lock"))
    }
}

/// The MusicBrainz request lines among `lines`.
fn musicbrainz(lines: &[String]) -> Vec<&String> {
    lines.iter().filter(|l| l.contains("/ws/2/")).collect()
}

/// The TheAudioDB request lines among `lines`.
fn audiodb(lines: &[String]) -> Vec<&String> {
    lines.iter().filter(|l| l.contains("-mb.php")).collect()
}

/// The searches (by name, not by id) among `lines`.
fn searches(lines: &[String]) -> Vec<&String> {
    lines.iter().filter(|l| l.contains("query=")).collect()
}

/// A scanner over `vf`'s libraries whose MusicBrainz and TheAudioDB answer
/// at `base`.
fn music_scanner(
    vf: &Arc<dyn VirtualFolderManager>,
    persistence: &Arc<FerrofinItemPersistenceService>,
    items: &Arc<dyn ItemRepository>,
    root: &Path,
    base: &str,
) -> LibraryScanner {
    LibraryScanner::new(
        Arc::clone(vf),
        Arc::new(FerrofinFileSystem::new()),
        persistence.clone(),
    )
    .with_music(
        Arc::new(ferrofin_providers::MusicBrainzClient::new(base, "test")),
        Arc::clone(items),
    )
    .with_audiodb(Arc::new(ferrofin_providers::AudioDbClient::with_base_url(
        base,
    )))
    .with_studio_images(Arc::new(ferrofin_providers::StudiosClient::with_repo_url(
        base,
    )))
    .with_metadata_dir(root.join("metadata"))
    .with_progress_every(0)
}

/// A music library of one artist folder with one album of two tracks,
/// scanned by a scanner with MusicBrainz and TheAudioDB wired.
struct Fixture {
    db: Database,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    music: Music,
    artist_dir: PathBuf,
    album_dir: PathBuf,
}

impl Fixture {
    async fn new(root: &Path, options: LibraryOptions) -> Self {
        Self::build(root, options, Vec::new(), true).await
    }

    /// [`new`](Self::new), with the plugins' metadata sources `plugins`
    /// registered on the scanner.
    async fn with_plugins(
        root: &Path,
        options: LibraryOptions,
        plugins: Vec<Arc<dyn DynamicMetadataProvider>>,
    ) -> Self {
        Self::build(root, options, plugins, true).await
    }

    /// The fixture's library, scanned by a scanner with the plugins'
    /// metadata sources `plugins` and — with `built_in` — MusicBrainz,
    /// TheAudioDB and the studio artwork wired (without it, no remote music
    /// provider of the server's own).
    async fn build(
        root: &Path,
        options: LibraryOptions,
        plugins: Vec<Arc<dyn DynamicMetadataProvider>>,
        built_in: bool,
    ) -> Self {
        let media = root.join("music");
        let artist_dir = media.join("Miles Davis");
        let album_dir = artist_dir.join("Kind of Blue");
        std::fs::create_dir_all(&album_dir).expect("mkdir");
        for track in ["01 - So What.mp3", "02 - Blue in Green.mp3"] {
            std::fs::write(album_dir.join(track), b"").expect("track");
        }
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Music",
            Some(CollectionTypeOptions::music),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..options
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let music = Music::spawn();
        let scanner = if built_in {
            music_scanner(&vf, &persistence, &items, root, &music.base)
        } else {
            LibraryScanner::new(
                Arc::clone(&vf),
                Arc::new(FerrofinFileSystem::new()),
                persistence.clone(),
            )
            .with_items(Arc::clone(&items))
            .with_metadata_dir(root.join("metadata"))
            .with_progress_every(0)
        }
        .with_dynamic_providers(plugins);
        Self {
            db,
            items,
            persistence,
            scanner,
            music,
            artist_dir,
            album_dir,
        }
    }

    async fn scan(&self) -> ScanOutcome {
        self.scanner.scan_all().await.expect("scan")
    }

    async fn scan_with(&self, options: &MetadataRefreshOptions) -> ScanOutcome {
        self.scanner.scan_with(None, options).await.expect("scan")
    }

    fn album_id(&self) -> Uuid {
        derive_item_id(BaseItemKind::MusicAlbum, &self.album_dir.to_string_lossy()).expect("id")
    }

    fn artist_id(&self) -> Uuid {
        derive_item_id(
            BaseItemKind::MusicArtist,
            &self.artist_dir.to_string_lossy(),
        )
        .expect("id")
    }

    async fn row(&self, id: Uuid) -> BaseItemEntity {
        self.items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row")
    }

    /// The item's stored `name` provider id.
    async fn id_of(&self, id: Uuid, name: &str) -> Option<String> {
        self.persistence
            .provider_ids_for_items(&[id])
            .await
            .expect("ids")
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// Locks `id` (`IsLocked`).
    async fn lock(&self, id: Uuid) {
        let row = self.row(id).await;
        self.persistence
            .save_items(&[BaseItemEntity {
                is_locked: true,
                ..row
            }])
            .await
            .expect("lock");
    }
}

/// The dashboard's "Replace all metadata".
fn replace_all() -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        remove_old_metadata: true,
        force_save: true,
        ..MetadataRefreshOptions::default()
    }
}

/// A first scan asks MusicBrainz and TheAudioDB for the album and the artist
/// (their first refresh), merges and stamps them; an unchanged rescan asks
/// nothing at all and writes neither.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_music_library_asks_no_provider() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    let first = fx.scan().await;
    assert!(
        first.created >= 4,
        "the artist, the album, 2 tracks: {first:?}"
    );
    let asked = fx.music.take();
    assert!(!musicbrainz(&asked).is_empty(), "{asked:?}");
    assert_eq!(audiodb(&asked).len(), 2, "album + artist: {asked:?}");
    let (album, artist) = (fx.row(fx.album_id()).await, fx.row(fx.artist_id()).await);
    assert_eq!(
        fx.id_of(fx.album_id(), "MusicBrainzAlbum").await.as_deref(),
        Some(RELEASE)
    );
    assert_eq!(
        fx.id_of(fx.artist_id(), "MusicBrainzArtist")
            .await
            .as_deref(),
        Some(ARTIST)
    );
    assert_eq!(album.overview.as_deref(), Some("Album text."));
    assert_eq!(album.production_year, Some(1959), "the release date");
    assert_eq!(artist.overview.as_deref(), Some("Artist bio."));
    assert!(album.date_last_refreshed.is_some() && artist.date_last_refreshed.is_some());

    let second = fx.scan().await;
    assert_eq!(second.created + second.updated, 0, "{second:?}");
    assert!(
        fx.music.take().is_empty(),
        "an unchanged rescan asks no provider"
    );
    let (album2, artist2) = (fx.row(fx.album_id()).await, fx.row(fx.artist_id()).await);
    assert_eq!(
        album2.date_last_saved, album.date_last_saved,
        "the album is not written"
    );
    assert_eq!(
        artist2.date_last_saved, artist.date_last_saved,
        "the artist is not written"
    );
}

/// A new album is refreshed on its own: it is searched and fetched, the
/// album already matched is neither searched nor refreshed.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_album_fetches_only_itself() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let old = fx.row(fx.album_id()).await;

    let new_dir = fx.artist_dir.join("Sketches of Spain");
    std::fs::create_dir_all(&new_dir).expect("mkdir");
    std::fs::write(new_dir.join("01 - Concierto.mp3"), b"").expect("track");
    let outcome = fx.scan().await;
    assert_eq!(outcome.created, 2, "the album and its track: {outcome:?}");
    let asked = fx.music.take();
    let searched = searches(&asked);
    assert_eq!(searched.len(), 1, "one search, the new album's: {asked:?}");
    assert!(searched[0].contains("Sketches"), "{searched:?}");
    let new_id = derive_item_id(BaseItemKind::MusicAlbum, &new_dir.to_string_lossy()).expect("id");
    assert!(fx.row(new_id).await.date_last_refreshed.is_some());
    let old_after = fx.row(fx.album_id()).await;
    assert_eq!(
        old_after.date_last_refreshed, old.date_last_refreshed,
        "not refreshed"
    );
    assert_eq!(
        old_after.date_last_saved, old.date_last_saved,
        "not written"
    );
}

/// The library monitor's scan of a new track in an existing album (the
/// watcher, a webhook): the album is the nearest existing item, so it — and
/// only it — refreshes, its music pass included; its artist above it is
/// context only: no provider is asked about it and its row is not written.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_track_reported_by_the_watcher_refreshes_only_its_album() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let (album, artist) = (fx.row(fx.album_id()).await, fx.row(fx.artist_id()).await);

    let track = fx.album_dir.join("03 - Freddie Freeloader.mp3");
    std::fs::write(&track, b"").expect("track");
    // The folder's mtime moves with the new file; make the drift unambiguous.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
    std::fs::File::open(&fx.album_dir)
        .expect("album dir")
        .set_modified(later)
        .expect("touch");
    let outcome = fx
        .scanner
        .scan_paths(&[track.to_string_lossy().into_owned()])
        .await
        .expect("scan");
    assert_eq!(outcome.created, 1, "the track: {outcome:?}");
    assert_eq!(outcome.updated, 1, "its album: {outcome:?}");
    let asked = fx.music.take();
    assert!(
        !asked.is_empty(),
        "the album's music refresh asked its providers"
    );
    assert!(
        asked
            .iter()
            .all(|l| !l.contains("/ws/2/artist") && !l.contains("artist-mb.php")),
        "nothing about the artist: {asked:?}"
    );
    assert!(
        searches(&asked).is_empty(),
        "by the stored match: {asked:?}"
    );
    assert_ne!(
        fx.row(fx.album_id()).await.date_last_refreshed,
        album.date_last_refreshed,
        "the album refreshed"
    );
    let artist_after = fx.row(fx.artist_id()).await;
    assert_eq!(
        artist_after.date_last_saved, artist.date_last_saved,
        "the artist is not written"
    );
    assert_eq!(artist_after.date_last_refreshed, artist.date_last_refreshed);
}

/// "Replace all metadata" runs every music provider again — by the stored
/// match, never a new search — and a locked album or artist stays out of it.
#[tokio::test(flavor = "multi_thread")]
async fn replace_all_refetches_by_the_stored_match_and_skips_a_locked_item() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();

    fx.scan_with(&replace_all()).await;
    let asked = fx.music.take();
    assert!(
        searches(&asked).is_empty(),
        "a stored match is not searched: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/release-group/{GROUP}"))),
        "the album is looked up again, dated by its release group: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/artist/{ARTIST}"))),
        "the artist is looked up again: {asked:?}"
    );
    assert_eq!(audiodb(&asked).len(), 2, "{asked:?}");
    let album = fx.row(fx.album_id()).await;
    assert_eq!(album.overview.as_deref(), Some("Album text."));

    fx.lock(fx.album_id()).await;
    fx.lock(fx.artist_id()).await;
    let edited = fx.row(fx.album_id()).await;
    fx.scan_with(&replace_all()).await;
    assert!(
        fx.music.take().is_empty(),
        "a locked album and artist ask nothing"
    );
    let after = fx.row(fx.album_id()).await;
    assert_eq!(after.overview, edited.overview);
    assert_eq!(after.name, edited.name);
}

/// A library whose "Metadata downloaders" and "Image fetchers" lists for
/// albums and artists are empty asks neither service — on a first scan, and
/// on "Replace all metadata" — and its albums are stamped by the walk.
#[tokio::test(flavor = "multi_thread")]
async fn a_librarys_unticked_music_fetchers_ask_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let cleared = |kind: &str| TypeOptions {
        type_: Some(kind.to_owned()),
        ..TypeOptions::default()
    };
    let fx = Fixture::new(
        tmp.path(),
        LibraryOptions {
            type_options: vec![cleared("MusicAlbum"), cleared("MusicArtist")],
            ..LibraryOptions::default()
        },
    )
    .await;
    fx.scan().await;
    assert!(fx.music.take().is_empty(), "every music fetcher unticked");
    assert!(fx.row(fx.album_id()).await.date_last_refreshed.is_some());
    assert!(fx.row(fx.artist_id()).await.date_last_refreshed.is_some());
    fx.scan_with(&replace_all()).await;
    assert!(fx.music.take().is_empty());
}

/// Owner decision D1: a music provider that FAILS leaves the item
/// unstamped, so the next scan asks again; one that answers stamps it, and
/// the scan after is quiet. The album's MusicBrainz requests fail while its
/// artist's answer: only the album is asked again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_music_provider_is_retried_on_the_next_scan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.music.fail(Some("/ws/2/release"));
    fx.scan().await;
    assert_eq!(fx.row(fx.album_id()).await.date_last_refreshed, None);
    assert!(fx.row(fx.artist_id()).await.date_last_refreshed.is_some());
    fx.music.take();

    fx.music.fail(None);
    let retried = fx.scan().await;
    assert!(retried.updated >= 1, "{retried:?}");
    let asked = fx.music.take();
    assert_eq!(searches(&asked).len(), 1, "the album, again: {asked:?}");
    assert!(
        !asked.iter().any(|l| l.contains("/ws/2/artist")),
        "the stamped artist is not asked: {asked:?}"
    );
    assert!(fx.row(fx.album_id()).await.date_last_refreshed.is_some());

    fx.scan().await;
    assert!(fx.music.take().is_empty(), "stamped: quiet");
}

/// D1 for a folder artist, whose search fails: it stays unstamped and is
/// asked again. Its album — no artist id (its artist's search failed) and
/// no album-artist tag — has nothing MusicBrainz can find it by: upstream's
/// provider then returns no metadata without asking (`MusicBrainzAlbumProvider.
/// cs:196-212`), which is no failure, so it is stamped.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_artist_search_is_retried_on_the_next_scan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.music.fail(Some("/ws/2/"));
    fx.scan().await;
    assert_eq!(fx.row(fx.artist_id()).await.date_last_refreshed, None);
    assert!(fx.row(fx.album_id()).await.date_last_refreshed.is_some());
    let asked = fx.music.take();
    assert!(
        !asked.iter().any(|l| l.contains("/ws/2/release")),
        "the album is never searched by its name alone: {asked:?}"
    );

    fx.music.fail(None);
    fx.scan().await;
    assert_eq!(
        fx.id_of(fx.artist_id(), "MusicBrainzArtist")
            .await
            .as_deref(),
        Some(ARTIST)
    );
    assert!(fx.row(fx.artist_id()).await.date_last_refreshed.is_some());
    fx.music.take();
    fx.scan().await;
    assert!(fx.music.take().is_empty(), "stamped: quiet");
}

/// `POST /Items/{id}/Refresh` of an artist known only by name runs through
/// the scanner (`ScanTarget::Artist` with no folder): a never-refreshed one
/// is searched and fetched; a Default refresh of it afterwards asks nothing;
/// "Search for missing metadata" asks again by the stored match; an Identify
/// fetches by the chosen id and makes it the artist's.
#[tokio::test(flavor = "multi_thread")]
// One artist through four refreshes in turn.
#[allow(clippy::too_many_lines)]
async fn a_by_name_artist_refreshes_through_the_scanner() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    // A compilation artist, known only by name.
    let track = derive_item_id(
        BaseItemKind::Audio,
        &fx.album_dir.join("01 - So What.mp3").to_string_lossy(),
    )
    .expect("id");
    fx.persistence
        .save_item_values(track, &[(1, "Gil Evans".into())])
        .await
        .expect("materialize");
    let by_name = fx
        .items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::MusicArtist],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("artists")
        .into_iter()
        .find(|a| a.name.as_deref() == Some("Gil Evans"))
        .expect("the by-name artist");
    let id = Uuid::parse_str(&by_name.id).expect("id");
    assert!(by_name.top_parent_id.as_deref().is_none_or(str::is_empty));
    let target = ScanTarget::Artist {
        id,
        path: None,
        folders: Vec::new(),
    };
    let refresh = |options: MetadataRefreshOptions| {
        let target = target.clone();
        let scanner = &fx.scanner;
        async move {
            let cancel = ScanCancel::new();
            scanner
                .scan_target(
                    &target,
                    ScanRun::new(&options, &options, &cancel).with_passes(ScanPasses::Touched),
                )
                .await
                .expect("refresh")
        }
    };

    let outcome = refresh(MetadataRefreshOptions::default()).await;
    assert_eq!(outcome.updated, 1, "{outcome:?}");
    let asked = fx.music.take();
    assert_eq!(searches(&asked).len(), 1, "searched by name: {asked:?}");
    let artist = fx.row(id).await;
    assert!(artist.date_last_refreshed.is_some());
    assert_eq!(artist.overview.as_deref(), Some("Artist bio."));
    assert_eq!(
        fx.id_of(id, "MusicBrainzArtist").await.as_deref(),
        Some(ARTIST)
    );

    let outcome = refresh(MetadataRefreshOptions::default()).await;
    assert_eq!(outcome.unchanged, 1, "{outcome:?}");
    assert!(
        fx.music.take().is_empty(),
        "refreshed already: nothing to ask"
    );

    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        force_save: true,
        ..MetadataRefreshOptions::default()
    };
    refresh(full).await;
    let asked = fx.music.take();
    assert!(searches(&asked).is_empty(), "{asked:?}");
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/artist/{ARTIST}"))),
        "{asked:?}"
    );
    assert_eq!(audiodb(&asked).len(), 1, "{asked:?}");

    let identify = MetadataRefreshOptions {
        search_result: Some(RemoteSearchResult {
            name: Some("Gil Evans".to_owned()),
            provider_ids: Some(
                [("MusicBrainzArtist".to_owned(), CHOSEN_ARTIST.to_owned())]
                    .into_iter()
                    .collect(),
            ),
            ..RemoteSearchResult::default()
        }),
        ..replace_all()
    };
    refresh(identify).await;
    let asked = fx.music.take();
    assert!(searches(&asked).is_empty(), "{asked:?}");
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/artist/{CHOSEN_ARTIST}"))),
        "looked up by the chosen id: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|l| l.contains("artist-mb.php") && l.contains(CHOSEN_ARTIST)),
        "{asked:?}"
    );
    assert_eq!(
        fx.id_of(id, "MusicBrainzArtist").await.as_deref(),
        Some(CHOSEN_ARTIST)
    );
}

/// "Identify → Apply" on an album is the scan of its folder with the chosen
/// result: its music providers re-fetch by the chosen release and release
/// group — never by the matched ones, never by a search — and the chosen
/// ids are the album's.
#[tokio::test(flavor = "multi_thread")]
async fn an_album_identify_refetches_by_the_chosen_ids() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let options = MetadataRefreshOptions {
        search_result: Some(RemoteSearchResult {
            name: Some("Kind of Blue".to_owned()),
            provider_ids: Some(
                [
                    ("MusicBrainzAlbum".to_owned(), CHOSEN_RELEASE.to_owned()),
                    (
                        "MusicBrainzReleaseGroup".to_owned(),
                        CHOSEN_GROUP.to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            ..RemoteSearchResult::default()
        }),
        ..replace_all()
    };
    let cancel = ScanCancel::new();
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    fx.scanner
        .scan_target(
            &ScanTarget::Paths(vec![fx.album_dir.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &none, &cancel).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("identify");
    let asked = fx.music.take();
    assert!(searches(&asked).is_empty(), "{asked:?}");
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/release-group/{CHOSEN_GROUP}"))),
        "the chosen release group is looked up: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|l| l.contains("album-mb.php") && l.contains(CHOSEN_GROUP)),
        "TheAudioDB by the chosen release group: {asked:?}"
    );
    assert!(
        !asked
            .iter()
            .any(|l| l.contains(RELEASE) || l.contains(GROUP)),
        "never the old match: {asked:?}"
    );
    assert_eq!(
        fx.id_of(fx.album_id(), "MusicBrainzAlbum").await.as_deref(),
        Some(CHOSEN_RELEASE)
    );
    assert_eq!(
        fx.id_of(fx.album_id(), "MusicBrainzReleaseGroup")
            .await
            .as_deref(),
        Some(CHOSEN_GROUP)
    );
}

/// A TV library of two series, scanned without providers.
async fn tv_library(root: &Path) -> (Database, Arc<dyn ItemRepository>, LibraryScanner, PathBuf) {
    let media = root.join("tv");
    for (show, episodes) in [("Alpha", 2), ("Bravo", 1)] {
        let season = media.join(show).join("Season 01");
        std::fs::create_dir_all(&season).expect("mkdir");
        for e in 1..=episodes {
            std::fs::write(season.join(format!("{show} - S01E0{e}.mkv")), b"x").expect("ep");
        }
    }
    std::fs::create_dir_all(media.join("Charlie").join("Season 01")).expect("empty series");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "TV",
        Some(CollectionTypeOptions::tvshows),
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
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::clone(&items))
        .with_progress_every(0);
    (db, items, scanner, media)
}

/// `UpdateDateLastMediaAdded` (`MetadataService.cs:509-541`) on the one
/// folder kind that supports it: a series' `DateLastMediaAdded` is the latest
/// `DateCreated` of its episodes, an empty series gets `DateTime.MinValue`
/// (stored `NULL`, as upstream's mapper does, `BaseItemMapper.cs:418`), a
/// new episode moves only its own series' date — and the "date added" sort
/// (`DateLastContentAdded`) orders the series by it.
#[tokio::test(flavor = "multi_thread")]
async fn a_series_last_media_date_follows_its_episodes_and_sorts_folders() {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    let tmp = tempfile::tempdir().expect("tmp");
    let (_db, items, scanner, media) = tv_library(tmp.path()).await;
    scanner.scan_all().await.expect("scan");

    let series = |name: &str| {
        derive_item_id(BaseItemKind::Series, &media.join(name).to_string_lossy()).expect("id")
    };
    let row = |id: Uuid| {
        let items = Arc::clone(&items);
        async move { items.retrieve_item(id).await.expect("read").expect("row") }
    };
    let latest_episode = |show: &'static str| {
        let items = Arc::clone(&items);
        let parent = series(show);
        async move {
            items
                .get_item_list(&InternalItemsQuery {
                    ancestor_ids: vec![parent],
                    include_item_types: vec![BaseItemKind::Episode],
                    recursive: true,
                    ..InternalItemsQuery::default()
                })
                .await
                .expect("episodes")
                .into_iter()
                .filter_map(|e| e.date_created)
                .max()
        }
    };
    let alpha = row(series("Alpha")).await;
    assert_eq!(alpha.date_last_media_added, latest_episode("Alpha").await);
    assert!(alpha.date_last_media_added.is_some());
    assert_eq!(
        row(series("Charlie")).await.date_last_media_added,
        None,
        "an empty series stores upstream's DateTime.MinValue as NULL"
    );

    // A new episode of Bravo, created after every other one.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        media
            .join("Bravo")
            .join("Season 01")
            .join("Bravo - S01E02.mkv"),
        b"x",
    )
    .expect("ep");
    scanner.scan_all().await.expect("rescan");
    let bravo = row(series("Bravo")).await;
    assert_eq!(bravo.date_last_media_added, latest_episode("Bravo").await);
    assert!(bravo.date_last_media_added > alpha.date_last_media_added);
    assert_eq!(
        row(series("Alpha")).await.date_last_media_added,
        alpha.date_last_media_added,
        "the other series is untouched"
    );

    let sorted: Vec<Option<String>> = items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Series],
            order_by: vec![(ItemSortBy::DateLastContentAdded, SortOrder::Descending)],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("sorted")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        sorted,
        vec![
            Some("Bravo".to_owned()),
            Some("Alpha".to_owned()),
            Some("Charlie".to_owned())
        ],
        "newest media first"
    );
}

/// The library-wide closing passes run on every library validation, each
/// by its own persisted selection: a `Year` row deleted behind the scan's
/// back is put back by the next rescan, though that rescan changes nothing
/// else.
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_rescan_still_runs_the_closing_passes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let heat = media.join("Heat (1995)").join("Heat (1995).mkv");
    std::fs::create_dir_all(heat.parent().expect("dir")).expect("mkdir");
    std::fs::write(&heat, b"x").expect("movie");
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
    let years = ferrofin_core::YearStore::new(
        persistence.clone(),
        ferrofin_core::item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: None,
        },
        tmp.path().join("metadata/Year"),
    );
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(items)
        .with_years(years)
        .with_progress_every(0);
    let count = || async {
        sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%Entities.Year'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("years")
    };
    scanner.scan_all().await.expect("scan");
    assert_eq!(count().await, 1);
    sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Type" LIKE '%Entities.Year'"#)
        .execute(db.writer())
        .await
        .expect("delete");
    let quiet = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        quiet.created + quiet.updated + quiet.removed,
        0,
        "{quiet:?}"
    );
    assert_eq!(count().await, 1, "the missing year is put back");
}

/// A library scan cancelled once its walk is done — at the 96 % mark,
/// before the pruning and the closing passes.
async fn scan_cancelled_after_the_walk(scanner: &LibraryScanner) -> ScanOutcome {
    let options = MetadataRefreshOptions::default();
    let cancel = ScanCancel::new();
    let stop = cancel.clone();
    let progress = move |percent: f64| {
        if percent >= 96.0 {
            stop.cancel();
        }
    };
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_progress(&progress),
        )
        .await
        .expect("scan")
}

/// The folder artist is refreshed before the album below it (upstream
/// refreshes a folder that is no `IMetadataContainer` before its children,
/// `Folder.cs:850-876`): the album's search, on a first scan of untagged
/// tracks, already names the artist's MusicBrainz id (`arid:`).
#[tokio::test(flavor = "multi_thread")]
async fn a_first_scan_searches_the_album_by_its_artists_id() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    let asked = fx.music.take();
    let artist = asked
        .iter()
        .position(|l| l.contains("/ws/2/artist?"))
        .expect("the artist is searched");
    let album = asked
        .iter()
        .position(|l| l.contains("/ws/2/release?"))
        .expect("the album is searched");
    assert!(artist < album, "the artist first: {asked:?}");
    assert!(
        asked[album].contains("arid") && asked[album].contains(ARTIST),
        "the album is searched by its artist's id: {asked:?}"
    );
    assert_eq!(
        fx.id_of(fx.album_id(), "MusicBrainzAlbum").await.as_deref(),
        Some(RELEASE)
    );
}

/// A folder change on an album left for the music pass survives a scan
/// stopped before that pass: the walk keeps the stored `DateModified` for
/// the music pass to write with its refresh (upstream's one `SaveInternal`,
/// `MetadataService.cs:244-255`), so the next scan still sees the change
/// and runs the album's providers.
#[tokio::test(flavor = "multi_thread")]
async fn an_album_change_survives_a_scan_cancelled_before_its_music_pass() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let before = fx.row(fx.album_id()).await;
    std::fs::File::open(&fx.album_dir)
        .expect("open the album folder")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(120))
        .expect("touch");

    let stopped = scan_cancelled_after_the_walk(&fx.scanner).await;
    assert!(stopped.stopped, "{stopped:?}");
    assert!(fx.music.take().is_empty(), "no music pass ran");
    assert_eq!(
        fx.row(fx.album_id()).await.date_modified,
        before.date_modified,
        "the change is still to be refreshed"
    );

    fx.scan().await;
    let asked = fx.music.take();
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/release-group/{GROUP}"))),
        "the album's providers run: {asked:?}"
    );
    let after = fx.row(fx.album_id()).await;
    assert_ne!(after.date_modified, before.date_modified, "written now");
    assert!(after.date_last_refreshed > before.date_last_refreshed);
    fx.scan().await;
    assert!(fx.music.take().is_empty(), "then quiet");
}

/// A retagged track changes nothing the album's providers own: with no
/// refresh of the album itself (upstream's `isFullRefresh || updateType >
/// None`, `AlbumMetadataService.cs:77-95`), the album keeps the
/// MusicBrainz first-release date rather than taking the track's.
#[tokio::test(flavor = "multi_thread")]
async fn a_retagged_track_keeps_the_albums_musicbrainz_date() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let album = fx.row(fx.album_id()).await;
    assert_eq!(album.production_year, Some(1959));
    let released = album.premiere_date;
    assert!(released.is_some());

    let path = fx.album_dir.join("01 - So What.mp3");
    let track = derive_item_id(BaseItemKind::Audio, &path.to_string_lossy()).expect("id");
    let tagged = chrono::DateTime::parse_from_rfc3339("2001-01-01T00:00:00Z")
        .expect("date")
        .with_timezone(&chrono::Utc);
    let row = fx.row(track).await;
    fx.persistence
        .save_items(&[BaseItemEntity {
            premiere_date: Some(tagged),
            production_year: Some(2001),
            ..row
        }])
        .await
        .expect("retag");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(120))
        .expect("touch");

    let outcome = fx.scan().await;
    assert_eq!(outcome.updated, 1, "the track: {outcome:?}");
    assert_eq!(
        fx.row(track).await.premiere_date,
        Some(tagged),
        "the track keeps its tag date"
    );
    let album = fx.row(fx.album_id()).await;
    assert_eq!(album.premiere_date, released, "the MusicBrainz date stays");
    assert_eq!(album.production_year, Some(1959));
}

/// An artist known only by name whose lookup failed is retried by the next
/// library validation — one that changes nothing else — and stamped once
/// it answers (`ArtistsValidator`'s `neverRefreshed`, D1).
#[tokio::test(flavor = "multi_thread")]
async fn a_by_name_artist_whose_lookup_failed_is_retried_by_a_quiet_rescan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let track = derive_item_id(
        BaseItemKind::Audio,
        &fx.album_dir.join("01 - So What.mp3").to_string_lossy(),
    )
    .expect("id");
    fx.persistence
        .save_item_values(track, &[(1, "Gil Evans".into())])
        .await
        .expect("materialize");
    let id = by_name_artist(&fx, "Gil Evans").await;

    fx.music.fail(Some("/ws/2/"));
    let quiet = fx.scan().await;
    assert_eq!(
        quiet.created + quiet.updated + quiet.removed,
        0,
        "{quiet:?}"
    );
    let asked = fx.music.take();
    assert_eq!(searches(&asked).len(), 1, "asked: {asked:?}");
    assert_eq!(
        fx.row(id).await.date_last_refreshed,
        None,
        "failed: unstamped"
    );

    fx.music.fail(None);
    let quiet = fx.scan().await;
    assert_eq!(
        quiet.created + quiet.updated + quiet.removed,
        0,
        "{quiet:?}"
    );
    assert_eq!(searches(&fx.music.take()).len(), 1, "asked again");
    assert!(fx.row(id).await.date_last_refreshed.is_some(), "stamped");
    assert_eq!(
        fx.id_of(id, "MusicBrainzArtist").await.as_deref(),
        Some(ARTIST)
    );

    fx.scan().await;
    assert!(fx.music.take().is_empty(), "then never again");
}

/// The id of the by-name `MusicArtist` row named `name`.
async fn by_name_artist(fx: &Fixture, name: &str) -> Uuid {
    let row = fx
        .items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::MusicArtist],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("artists")
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .expect("the by-name artist");
    assert!(row.top_parent_id.as_deref().is_none_or(str::is_empty));
    Uuid::parse_str(&row.id).expect("id")
}

/// A [`PriorityLane`] that serves one refresh of the fixture's album with
/// `options` once the scan serving it has saved the album — after its walk
/// decided the album's music refresh, before that refresh runs.
struct RefreshTheWalkedAlbum {
    items: Arc<dyn ItemRepository>,
    album: Uuid,
    album_dir: PathBuf,
    saved_before: Option<chrono::DateTime<chrono::Utc>>,
    options: MetadataRefreshOptions,
    served: Mutex<Vec<Result<ScanOutcome, String>>>,
    fired: Mutex<bool>,
}

impl RefreshTheWalkedAlbum {
    /// The lane for `fx`'s album, armed from its stored `DateLastSaved` now.
    async fn new(fx: &Fixture, options: MetadataRefreshOptions) -> Self {
        Self {
            items: Arc::clone(&fx.items),
            album: fx.album_id(),
            album_dir: fx.album_dir.clone(),
            saved_before: fx.row(fx.album_id()).await.date_last_saved,
            options,
            served: Mutex::new(Vec::new()),
            fired: Mutex::new(false),
        }
    }
}

/// The options of "Identify → Apply" choosing [`CHOSEN_RELEASE`] in
/// [`CHOSEN_GROUP`].
fn identify_chosen() -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        search_result: Some(RemoteSearchResult {
            name: Some("Kind of Blue".to_owned()),
            provider_ids: Some(
                [
                    ("MusicBrainzAlbum".to_owned(), CHOSEN_RELEASE.to_owned()),
                    (
                        "MusicBrainzReleaseGroup".to_owned(),
                        CHOSEN_GROUP.to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            ..RemoteSearchResult::default()
        }),
        ..MetadataRefreshOptions::default()
    }
}

impl ferrofin_core::PriorityLane for RefreshTheWalkedAlbum {
    fn next(&self) -> Option<ferrofin_core::LaneRefresh> {
        let mut fired = self.fired.lock().expect("lock");
        if *fired {
            return None;
        }
        let items = Arc::clone(&self.items);
        let album = self.album;
        let saved = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                items
                    .retrieve_item(album)
                    .await
                    .expect("read")
                    .and_then(|row| row.date_last_saved)
            })
        });
        if saved <= self.saved_before {
            return None;
        }
        *fired = true;
        let none = MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::None,
            image_refresh_mode: MetadataRefreshMode::None,
            ..MetadataRefreshOptions::default()
        };
        Some(ferrofin_core::LaneRefresh {
            target: ScanTarget::Paths(vec![self.album_dir.to_string_lossy().into_owned()]),
            options: self.options.clone(),
            ancestors: none,
            passes: ScanPasses::Touched,
            cancel: ScanCancel::new(),
            key: 1,
        })
    }

    fn done(
        &self,
        _refresh: ferrofin_core::LaneRefresh,
        outcome: &Result<ScanOutcome, ferrofin_traits::error::ServiceError>,
    ) {
        let outcome = outcome.as_ref().copied().map_err(ToString::to_string);
        self.served.lock().expect("lock").push(outcome);
    }
}

/// An Identify of an album served inside a scan after the walk decided the
/// album's refresh: the Identify's own refresh makes the chosen release the
/// album's, and the scan's music pass — decided before it — leaves the
/// album alone rather than taking its tracks' release back
/// (`SetProviderIdFromSongs`).
#[tokio::test(flavor = "multi_thread")]
async fn an_identify_served_after_the_walk_decided_the_album_survives_the_scan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    // Both tracks carry the matched release in their tags, and the album is
    // due a first refresh again.
    for track in ["01 - So What.mp3", "02 - Blue in Green.mp3"] {
        let id = derive_item_id(
            BaseItemKind::Audio,
            &fx.album_dir.join(track).to_string_lossy(),
        )
        .expect("id");
        fx.persistence
            .save_provider_id(id, "MusicBrainzAlbum", RELEASE)
            .await
            .expect("tag");
    }
    let album = fx.row(fx.album_id()).await;
    fx.persistence
        .save_items(&[BaseItemEntity {
            date_last_refreshed: None,
            ..album
        }])
        .await
        .expect("unstamp");

    let lane = RefreshTheWalkedAlbum::new(&fx, identify_chosen()).await;
    let options = MetadataRefreshOptions::default();
    let cancel = ScanCancel::new();
    fx.scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_lane(&lane),
        )
        .await
        .expect("scan");
    let served = lane.served.lock().expect("lock").clone();
    assert_eq!(served.len(), 1, "the Identify was served: {served:?}");
    assert!(served[0].is_ok(), "{served:?}");
    assert_eq!(
        fx.id_of(fx.album_id(), "MusicBrainzAlbum").await.as_deref(),
        Some(CHOSEN_RELEASE),
        "the chosen release survives the scan"
    );
    assert_eq!(
        fx.id_of(fx.album_id(), "MusicBrainzReleaseGroup")
            .await
            .as_deref(),
        Some(CHOSEN_GROUP)
    );
}

/// A new episode saved by a scan stopped before its closing passes: the
/// next scan — which saves nothing — derives the series'
/// `DateLastMediaAdded` from it, as upstream's refresh of the series does
/// on every scan.
#[tokio::test(flavor = "multi_thread")]
async fn a_series_last_media_date_is_derived_after_a_cancelled_scan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (_db, items, scanner, media) = tv_library(tmp.path()).await;
    scanner.scan_all().await.expect("scan");
    let bravo =
        derive_item_id(BaseItemKind::Series, &media.join("Bravo").to_string_lossy()).expect("id");
    let before = items
        .retrieve_item(bravo)
        .await
        .expect("read")
        .expect("row")
        .date_last_media_added;
    assert!(before.is_some());

    std::thread::sleep(std::time::Duration::from_millis(20));
    let episode = media
        .join("Bravo")
        .join("Season 01")
        .join("Bravo - S01E02.mkv");
    std::fs::write(&episode, b"x").expect("ep");
    let stopped = scan_cancelled_after_the_walk(&scanner).await;
    assert!(stopped.stopped && stopped.created == 1, "{stopped:?}");
    let new = derive_item_id(BaseItemKind::Episode, &episode.to_string_lossy()).expect("id");
    let created = items
        .retrieve_item(new)
        .await
        .expect("read")
        .expect("the episode was saved")
        .date_created;
    assert_eq!(
        items
            .retrieve_item(bravo)
            .await
            .expect("read")
            .expect("row")
            .date_last_media_added,
        before,
        "not derived: the passes never ran"
    );

    let quiet = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        quiet.created + quiet.updated + quiet.removed,
        0,
        "{quiet:?}"
    );
    assert_eq!(
        items
            .retrieve_item(bravo)
            .await
            .expect("read")
            .expect("row")
            .date_last_media_added,
        created,
        "derived by the next scan"
    );
}

/// What a write outside the scanner leaves the closing passes — the metadata
/// editor's new genre, studio and album artist (`save_item_values` makes
/// their by-name rows) and a new playlist — is done by the next library
/// validation, though that validation saves nothing: the genre gets its
/// path and its collage, the studio its artwork lookup, the artist its
/// first refresh, the playlist its collage.
// One library, one editor write, one quiet rescan, four passes checked.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_rescan_does_the_work_an_edit_and_a_new_playlist_left() {
    use ferrofin_model::entities::ImageType;
    use ferrofin_traits::persistence::LinkedChildrenService as _;
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let dir = media.join("Heat (1995)");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("Heat (1995).mkv"), b"x").expect("movie");
    let mut poster = image::RgbImage::new(40, 60);
    for px in poster.pixels_mut() {
        *px = image::Rgb([180, 30, 30]);
    }
    poster.save(dir.join("poster.png")).expect("poster");
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
    let meta = tmp.path().join("metadata");
    let music = Music::spawn();
    let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> =
        Arc::new(ferrofin_drawing::ImageProcessor::new(
            Arc::new(ferrofin_drawing::ImageCrateEncoder::new()),
            tmp.path().join("cache"),
        ));
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
        .with_music(
            Arc::new(ferrofin_providers::MusicBrainzClient::new(
                &music.base,
                "test",
            )),
            Arc::clone(&items),
        )
        .with_image_processor(processor)
        .with_metadata_dir(meta.join("library"))
        .with_by_name_store(ferrofin_core::by_name_store::ByNameStore::new(
            persistence.clone(),
            meta.join("Genre"),
            meta.join("MusicGenre"),
            meta.join("Studio"),
            meta.join("artists"),
        ))
        .with_studio_images(Arc::new(ferrofin_providers::StudiosClient::with_repo_url(
            &music.base,
        )))
        .with_progress_every(0);
    scanner.scan_all().await.expect("scan");
    let quiet = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        quiet.created + quiet.updated + quiet.removed,
        0,
        "{quiet:?}"
    );
    music.take();

    // The editor's save of the movie, and a new playlist of it.
    let movie = derive_item_id(
        BaseItemKind::Movie,
        &dir.join("Heat (1995).mkv").to_string_lossy(),
    )
    .expect("id");
    persistence
        .save_item_values(
            movie,
            &[
                (1, "Gil Evans".into()),
                (2, "Noir".into()),
                (3, "Mosfilm".into()),
            ],
        )
        .await
        .expect("edit");
    let playlist = Uuid::from_u128(0xFEED);
    persistence
        .save_items(&[BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(playlist),
            type_: ferrofin_core::item_type_lookup::stored_type_name(BaseItemKind::Playlist)
                .expect("type")
                .to_owned(),
            name: Some("Mix".to_owned()),
            is_folder: true,
            ..BaseItemEntity::default()
        }])
        .await
        .expect("playlist");
    ferrofin_core::FerrofinLinkedChildrenService::new(db.clone())
        .upsert_linked_child(playlist, movie, 0)
        .await
        .expect("member");

    let quiet = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        quiet.created + quiet.updated + quiet.removed,
        0,
        "{quiet:?}"
    );
    let named = |kind: BaseItemKind, name: &'static str| {
        let items = Arc::clone(&items);
        async move {
            items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    name: Some(name.to_owned()),
                    ..InternalItemsQuery::default()
                })
                .await
                .expect("query")
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("the {kind:?} {name}"))
        }
    };
    let primary = |id: &str| {
        let items = Arc::clone(&items);
        let id = Uuid::parse_str(id).expect("id");
        async move {
            items
                .get_image_infos(id)
                .await
                .expect("images")
                .into_iter()
                .any(|i| i.image_type == ImageType::Primary)
        }
    };
    let genre = named(BaseItemKind::Genre, "Noir").await;
    assert!(genre.path.is_some(), "the genre's path is filled");
    assert!(primary(&genre.id).await, "the genre's collage is drawn");
    let studio = named(BaseItemKind::Studio, "Mosfilm").await;
    assert!(
        studio.date_last_refreshed.is_some(),
        "the studio's artwork was looked up"
    );
    let artist = named(BaseItemKind::MusicArtist, "Gil Evans").await;
    assert!(
        artist.date_last_refreshed.is_some(),
        "the by-name artist had its first refresh"
    );
    assert_eq!(searches(&music.take()).len(), 1, "the artist's search");
    assert!(
        primary(&ferrofin_db::store::guid_to_db(playlist)).await,
        "the playlist's collage is drawn"
    );
}

/// `MusicBrainzAlbumProvider.Populate` and the artist provider's lookup,
/// ahead of TheAudioDB (MusicBrainz's `Order` 0): the album takes the
/// release group's first release date over the release's (a reissue), its
/// genres and tags by votes, its credits as album artists and its labels
/// as studios; TheAudioDB's single genre fills nothing MusicBrainz filled.
/// The artist takes its genres, tags and area.
#[tokio::test(flavor = "multi_thread")]
async fn musicbrainz_populates_albums_and_artists_ahead_of_theaudiodb() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    let album = fx.row(fx.album_id()).await;
    assert_eq!(
        album.production_year,
        Some(1959),
        "the group's first release"
    );
    assert_eq!(album.genres.as_deref(), Some("jazz|modal jazz"), "by votes");
    assert_eq!(album.tags.as_deref(), Some("trumpet"));
    assert_eq!(album.studios.as_deref(), Some("Columbia"));
    assert_eq!(album.album_artists.as_deref(), Some("Miles Davis"));
    assert_eq!(
        album.overview.as_deref(),
        Some("Album text."),
        "TheAudioDB fills what MusicBrainz left"
    );
    let artist = fx.row(fx.artist_id()).await;
    assert_eq!(artist.genres.as_deref(), Some("jazz"));
    assert_eq!(artist.tags.as_deref(), Some("trumpeter"));
    assert_eq!(
        artist.production_locations.as_deref(),
        Some("United States")
    );
    assert_eq!(artist.overview.as_deref(), Some("Artist bio."));
}

/// The provider name of [`PluginDb`].
const PLUGIN: &str = "PluginDb";

/// A plugin's metadata source for music: it answers for an album and for an
/// artist with genres and an overview (which MusicBrainz and TheAudioDB
/// answer too) and a tagline (which neither does), for a track with nothing,
/// and records the kind and name of every item it is asked about.
struct PluginDb {
    asked: Mutex<Vec<(String, String)>>,
}

impl PluginDb {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
        })
    }

    /// The (kind, name) of every item asked about since the last call.
    fn take(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.asked.lock().expect("lock"))
    }

    /// As a registered source.
    fn source(self: &Arc<Self>) -> Vec<Arc<dyn DynamicMetadataProvider>> {
        vec![Arc::clone(self) as Arc<dyn DynamicMetadataProvider>]
    }
}

#[async_trait::async_trait]
impl DynamicMetadataProvider for PluginDb {
    fn name(&self) -> &'static str {
        PLUGIN
    }

    fn library_gated(&self) -> bool {
        true
    }

    async fn lookup(
        &self,
        item: &DynamicMetadataLookup,
    ) -> Result<Option<DynamicMetadataResult>, ferrofin_traits::error::ServiceError> {
        self.asked
            .lock()
            .expect("lock")
            .push((item.kind.clone(), item.name.clone()));
        let (overview, genre) = match item.kind.as_str() {
            "MusicAlbum" => ("PluginDb album text.", "Cool jazz"),
            "MusicArtist" => ("PluginDb artist text.", "Hard bop"),
            _ => return Ok(None),
        };
        Ok(Some(DynamicMetadataResult {
            overview: Some(overview.to_owned()),
            genres: vec![genre.to_owned()],
            tagline: Some("PluginDb tagline.".to_owned()),
            ..DynamicMetadataResult::default()
        }))
    }
}

/// A library that ticked MusicBrainz, TheAudioDB and [`PluginDb`] for albums
/// and artists and saved `order` as their order.
fn music_order(order: &[&str]) -> LibraryOptions {
    use ferrofin_providers::library_options::fetcher_names::{AUDIODB, MUSICBRAINZ};
    let entry = |kind: &str| TypeOptions {
        type_: Some(kind.to_owned()),
        metadata_fetchers: vec![
            MUSICBRAINZ.to_owned(),
            AUDIODB.to_owned(),
            PLUGIN.to_owned(),
        ],
        metadata_fetcher_order: order.iter().map(|n| (*n).to_owned()).collect(),
        ..TypeOptions::default()
    };
    LibraryOptions {
        type_options: vec![entry("MusicAlbum"), entry("MusicArtist")],
        ..LibraryOptions::default()
    }
}

/// [`music_order`] with the plugin ranked first, or left out of the order.
fn plugin_ranked(first: bool) -> LibraryOptions {
    use ferrofin_providers::library_options::fetcher_names::{AUDIODB, MUSICBRAINZ};
    if first {
        music_order(&[PLUGIN, MUSICBRAINZ, AUDIODB])
    } else {
        music_order(&[MUSICBRAINZ, AUDIODB])
    }
}

/// A plugin's metadata source runs among an album's providers at its rank
/// in the library's order, like any provider (`ExecuteRemoteProviders`):
/// ranked before MusicBrainz and TheAudioDB its genres and overview win;
/// left out of the order it ranks by its `IHasOrder` (50), after both, and
/// only fills what they left (the tagline). An album asks it once, in the
/// music pass, never also in the walk.
#[rstest::rstest]
#[case::ranked_first(true)]
#[case::unranked(false)]
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_source_runs_at_its_rank_among_an_albums_providers(#[case] first: bool) {
    let plugin = PluginDb::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::with_plugins(tmp.path(), plugin_ranked(first), plugin.source()).await;
    fx.scan().await;
    let album = fx.row(fx.album_id()).await;
    if first {
        assert_eq!(album.genres.as_deref(), Some("Cool jazz"));
        assert_eq!(album.overview.as_deref(), Some("PluginDb album text."));
    } else {
        assert_eq!(album.genres.as_deref(), Some("jazz|modal jazz"));
        assert_eq!(album.overview.as_deref(), Some("Album text."));
    }
    assert_eq!(album.tagline.as_deref(), Some("PluginDb tagline."));
    assert_eq!(
        album.production_year,
        Some(1959),
        "MusicBrainz's year fills"
    );
    assert_eq!(
        plugin
            .take()
            .iter()
            .filter(|(kind, _)| kind == "MusicAlbum")
            .count(),
        1,
        "the album asks its providers once, in the music pass"
    );
}

/// The same for a folder artist's providers: ranked first, the plugin's
/// overview and genres win over TheAudioDB's biography and MusicBrainz's
/// genres; unranked, theirs stand and the plugin fills the tagline.
#[rstest::rstest]
#[case::ranked_first(true)]
#[case::unranked(false)]
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_source_runs_at_its_rank_among_a_folder_artists_providers(#[case] first: bool) {
    let plugin = PluginDb::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::with_plugins(tmp.path(), plugin_ranked(first), plugin.source()).await;
    fx.scan().await;
    let artist = fx.row(fx.artist_id()).await;
    if first {
        assert_eq!(artist.overview.as_deref(), Some("PluginDb artist text."));
        assert_eq!(artist.genres.as_deref(), Some("Hard bop"));
    } else {
        assert_eq!(artist.overview.as_deref(), Some("Artist bio."));
        assert_eq!(artist.genres.as_deref(), Some("jazz"));
    }
    assert_eq!(artist.tagline.as_deref(), Some("PluginDb tagline."));
    assert!(
        plugin
            .take()
            .contains(&("MusicArtist".to_owned(), "Miles Davis".to_owned())),
        "asked by the artist's name"
    );
}

/// A library whose only metadata source for music is a plugin (neither
/// MusicBrainz nor TheAudioDB wired) still runs the music pass's providers
/// for its album and artist: the plugin's answer is all they get.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_only_music_library_still_asks_the_plugin() {
    let plugin = PluginDb::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::build(
        tmp.path(),
        LibraryOptions::default(),
        plugin.source(),
        false,
    )
    .await;
    fx.scan().await;
    let album = fx.row(fx.album_id()).await;
    assert_eq!(album.overview.as_deref(), Some("PluginDb album text."));
    assert_eq!(album.genres.as_deref(), Some("Cool jazz"));
    let artist = fx.row(fx.artist_id()).await;
    assert_eq!(artist.overview.as_deref(), Some("PluginDb artist text."));
    assert!(
        fx.music.take().is_empty(),
        "no built-in music provider wired"
    );
}

/// A plugin is asked once per refresh that runs the providers and never by
/// an unchanged rescan: a first scan asks it about each track (the walk),
/// the album and the artist; an unchanged rescan asks nothing; an artist
/// known only by name (`ArtistsValidator`) is asked by its name on the next
/// validation, with the server-wide options (no library): unranked, the
/// plugin fills what MusicBrainz and TheAudioDB left; once stamped, a quiet
/// rescan asks nothing again.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_is_never_asked_by_an_unchanged_rescan() {
    let plugin = PluginDb::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::with_plugins(tmp.path(), LibraryOptions::default(), plugin.source()).await;
    fx.scan().await;
    let mut first: Vec<String> = plugin.take().into_iter().map(|(kind, _)| kind).collect();
    first.sort();
    assert_eq!(first, ["Audio", "Audio", "MusicAlbum", "MusicArtist"]);

    fx.scan().await;
    assert_eq!(plugin.take(), [], "an unchanged rescan asks no plugin");

    let track = derive_item_id(
        BaseItemKind::Audio,
        &fx.album_dir.join("01 - So What.mp3").to_string_lossy(),
    )
    .expect("id");
    fx.persistence
        .save_item_values(track, &[(1, "Gil Evans".into())])
        .await
        .expect("materialize");
    let gil = by_name_artist(&fx, "Gil Evans").await;
    fx.scan().await;
    assert_eq!(
        plugin.take(),
        [("MusicArtist".to_owned(), "Gil Evans".to_owned())],
        "the by-name artist's first refresh asks it"
    );
    let artist = fx.row(gil).await;
    assert_eq!(artist.tagline.as_deref(), Some("PluginDb tagline."));
    assert_eq!(artist.overview.as_deref(), Some("Artist bio."), "unranked");

    fx.scan().await;
    assert_eq!(plugin.take(), [], "a stamped by-name artist asks nothing");
}

/// An `album.nfo` and an `artist.nfo` are the items' local metadata, merged
/// into `temp` before any remote provider (`MetadataService.cs:803-850`), so
/// MusicBrainz, TheAudioDB and a plugin ranked first only fill what the NFO
/// left: the NFO's overview and genres stand over all three; the plugin's
/// tagline and MusicBrainz's year fill the gaps. "Replace all metadata"
/// keeps it so — upstream's local readers still seed `temp` there unless the
/// item has a metadata saver (`isSavingMetadata`, `:803`), and Ferrofin
/// runs none — and the NFO's values replace the stored ones.
#[rstest::rstest]
#[case::first_scan(false)]
#[case::replace_all(true)]
#[tokio::test(flavor = "multi_thread")]
async fn the_music_nfo_stands_over_every_remote_provider(#[case] replace: bool) {
    let plugin = PluginDb::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::with_plugins(tmp.path(), plugin_ranked(true), plugin.source()).await;
    std::fs::write(
        fx.album_dir.join("album.nfo"),
        "<album><title>Kind of Blue</title><plot>NFO album text.</plot>\
         <genre>Bebop</genre></album>",
    )
    .expect("album.nfo");
    std::fs::write(
        fx.artist_dir.join("artist.nfo"),
        "<artist><biography>NFO artist bio.</biography><genre>Trumpet jazz</genre></artist>",
    )
    .expect("artist.nfo");
    fx.scan().await;
    if replace {
        fx.scan_with(&replace_all()).await;
    }
    let album = fx.row(fx.album_id()).await;
    assert_eq!(album.overview.as_deref(), Some("NFO album text."));
    assert_eq!(album.genres.as_deref(), Some("Bebop"));
    assert_eq!(album.tagline.as_deref(), Some("PluginDb tagline."));
    assert_eq!(
        album.production_year,
        Some(1959),
        "MusicBrainz fills the year"
    );
    let artist = fx.row(fx.artist_id()).await;
    assert_eq!(artist.overview.as_deref(), Some("NFO artist bio."));
    assert_eq!(artist.genres.as_deref(), Some("Trumpet jazz"));
    assert_eq!(artist.tagline.as_deref(), Some("PluginDb tagline."));
    assert!(
        plugin.take().iter().any(|(kind, _)| kind == "MusicAlbum"),
        "the plugin still answered"
    );
}

/// "Identify → Apply" on a folder artist refreshes the albums below it with
/// the same options, chosen result included (`ProviderManager.RefreshItem`
/// → `ValidateChildren(options)`), and a refresh with a search result runs
/// no local reader (`options.SearchResult is null`, `MetadataService.cs:
/// 803`) — the identified artist's nor its albums'. So the album's
/// `album.nfo` does not start its providers' `temp`, and under the
/// Identify's "Replace all metadata" the providers' overview and genres
/// replace the NFO's that the first scan saved.
#[tokio::test(flavor = "multi_thread")]
async fn an_album_under_an_identified_artist_reads_no_nfo() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    std::fs::write(
        fx.album_dir.join("album.nfo"),
        "<album><title>Kind of Blue</title><plot>NFO album text.</plot>\
         <genre>Bebop</genre></album>",
    )
    .expect("album.nfo");
    fx.scan().await;
    let album = fx.row(fx.album_id()).await;
    assert_eq!(album.overview.as_deref(), Some("NFO album text."));
    assert_eq!(album.genres.as_deref(), Some("Bebop"));

    let options = MetadataRefreshOptions {
        search_result: Some(RemoteSearchResult {
            name: Some("Miles Davis".to_owned()),
            provider_ids: Some(
                [("MusicBrainzArtist".to_owned(), CHOSEN_ARTIST.to_owned())]
                    .into_iter()
                    .collect(),
            ),
            ..RemoteSearchResult::default()
        }),
        ..replace_all()
    };
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    let cancel = ScanCancel::new();
    fx.scanner
        .scan_target(
            &ScanTarget::Paths(vec![fx.artist_dir.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &none, &cancel).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("identify");
    assert_eq!(
        fx.id_of(fx.artist_id(), "MusicBrainzArtist")
            .await
            .as_deref(),
        Some(CHOSEN_ARTIST),
        "the artist took the chosen id"
    );
    let album = fx.row(fx.album_id()).await;
    assert_eq!(
        album.overview.as_deref(),
        Some("Album text."),
        "TheAudioDB's"
    );
    assert_eq!(
        album.genres.as_deref(),
        Some("jazz|modal jazz"),
        "MusicBrainz's"
    );
}

/// A refresh of an album served inside a scan after the walk decided the
/// album's refresh leaves the album to that refresh only when it went as
/// far as the scan's own: a Default refresh served inside a "Search for
/// missing metadata" scan does not spare the album the scan's providers.
#[tokio::test(flavor = "multi_thread")]
async fn a_weaker_refresh_served_inside_a_scan_does_not_spare_the_album() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    fx.scan().await;
    fx.music.take();
    let lane = RefreshTheWalkedAlbum::new(&fx, MetadataRefreshOptions::default()).await;
    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    let cancel = ScanCancel::new();
    fx.scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&full, &full, &cancel).with_lane(&lane),
        )
        .await
        .expect("scan");
    assert_eq!(lane.served.lock().expect("lock").len(), 1, "served");
    let asked = fx.music.take();
    assert!(
        asked
            .iter()
            .any(|l| l.contains(&format!("/ws/2/release-group/{GROUP}"))),
        "the scan's own refresh of the album ran: {asked:?}"
    );
}

/// A row that cannot be decoded skips itself only: the album the walk
/// could not read gets no music refresh (and does not stop the ones after
/// it), and an unreadable never-refreshed by-name artist or studio does
/// not stop its pass — the others are refreshed and stamped.
#[tokio::test(flavor = "multi_thread")]
// One library, three corrupted rows, one scan.
#[allow(clippy::too_many_lines)]
async fn a_row_that_cannot_be_read_skips_itself_only() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    // A second album, walked after "Kind of Blue".
    let sketches = fx.artist_dir.join("Sketches of Spain");
    std::fs::create_dir_all(&sketches).expect("mkdir");
    std::fs::write(sketches.join("01 - Concierto.mp3"), b"").expect("track");
    fx.scan().await;
    let sketches_id =
        derive_item_id(BaseItemKind::MusicAlbum, &sketches.to_string_lossy()).expect("id");
    let before = fx.row(sketches_id).await.date_last_refreshed;

    // New by-name artists and studios, as the metadata editor makes them.
    let track = derive_item_id(
        BaseItemKind::Audio,
        &fx.album_dir.join("01 - So What.mp3").to_string_lossy(),
    )
    .expect("id");
    fx.persistence
        .save_item_values(
            track,
            &[
                (1, "Gil Evans".into()),
                (1, "Bill Evans".into()),
                (1, "Tadd Dameron".into()),
                (3, "Blue Note".into()),
                (3, "Prestige".into()),
                (3, "Riverside".into()),
            ],
        )
        .await
        .expect("edit");
    // Corrupt the album walked first, and the first of each selection.
    let corrupt = |id: String| {
        let db = fx.db.clone();
        async move {
            sqlx::query(r#"UPDATE "BaseItems" SET "DateCreated" = 'not a date' WHERE "Id" = ?1"#)
                .bind(id)
                .execute(db.writer())
                .await
                .expect("corrupt a row");
        }
    };
    corrupt(ferrofin_db::store::guid_to_db(fx.album_id())).await;
    let first_never_refreshed = |pattern: &'static str| {
        let db = fx.db.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                r#"SELECT "Id" FROM "BaseItems"
                   WHERE "Type" LIKE ?1 AND "DateLastRefreshed" IS NULL
                   ORDER BY "Id" LIMIT 1"#,
            )
            .bind(pattern)
            .fetch_one(db.pool())
            .await
            .expect("a never-refreshed row")
        }
    };
    let bad_artist = first_never_refreshed("%Audio.MusicArtist").await;
    let bad_studio = first_never_refreshed("%Entities.Studio").await;
    corrupt(bad_artist.clone()).await;
    corrupt(bad_studio.clone()).await;
    assert!(fx.items.retrieve_item(fx.album_id()).await.is_err());

    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    let outcome = fx.scan_with(&full).await;
    assert!(!outcome.stopped, "{outcome:?}");
    assert!(
        fx.row(sketches_id).await.date_last_refreshed > before,
        "the album after the unreadable one is refreshed"
    );
    let stamped: Vec<(String, String, bool)> = sqlx::query_as::<_, (String, String, Option<String>)>(
        r#"SELECT "Id", "Name", "DateLastRefreshed" FROM "BaseItems"
           WHERE ("Type" LIKE '%Audio.MusicArtist' AND ("TopParentId" IS NULL OR "TopParentId" = ''))
              OR "Type" LIKE '%Entities.Studio'"#,
    )
    .fetch_all(fx.db.pool())
    .await
    .expect("rows")
    .into_iter()
    .map(|(id, name, refreshed)| (id, name, refreshed.is_some()))
    .collect();
    for (id, name, refreshed) in &stamped {
        let unreadable = *id == bad_artist || *id == bad_studio;
        assert_eq!(
            *refreshed, !unreadable,
            "{name}: only the unreadable rows stay unstamped ({stamped:?})"
        );
    }
    assert!(stamped.len() >= 6, "{stamped:?}");
}

/// The music pass writes an item's row — its `DateLastRefreshed` stamp
/// with it — after everything else it owns: a write that fails first (here
/// the album's provider ids) leaves the album unstamped and its row as it
/// was, so the next scan refreshes it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_write_leaves_the_album_due() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), LibraryOptions::default()).await;
    let album = ferrofin_db::store::guid_to_db(fx.album_id());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"CREATE TRIGGER "test_refuse_ids" BEFORE INSERT ON "BaseItemProviders"
           WHEN NEW."ItemId" = '{album}'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#
    )))
    .execute(fx.db.writer())
    .await
    .expect("trigger");
    fx.scan().await;
    let row = fx.row(fx.album_id()).await;
    assert_eq!(row.date_last_refreshed, None, "unstamped: still due");
    assert_eq!(row.production_year, None, "the row was not written");
    assert!(
        fx.row(fx.artist_id()).await.date_last_refreshed.is_some(),
        "the other items go on"
    );

    sqlx::query(r#"DROP TRIGGER "test_refuse_ids""#)
        .execute(fx.db.writer())
        .await
        .expect("drop trigger");
    fx.scan().await;
    let row = fx.row(fx.album_id()).await;
    assert!(row.date_last_refreshed.is_some(), "stamped once written");
    assert_eq!(row.production_year, Some(1959));
}
