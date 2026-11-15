//! The artwork a Kodi NFO declares (gl-74): an `<actor><thumb>` photo, and
//! an item's `<thumb>` and `<fanart><thumb>` URLs.
//!
//! Upstream's NFO reader keeps an actor's `<thumb>` as the credit's
//! `PersonInfo.ImageUrl` (`XmlReaderExtensions.GetPersonFromXmlNode`), and
//! `LibraryManager.SavePeopleMetadataAsync` gives it to a person who has no
//! Primary image yet. The item's own artwork URLs are its
//! `localItem.RemoteImages` (`BaseNfoParser.FetchThumbNode`), which
//! `MetadataService.RefreshWithProviders` saves for each type the item has
//! no image of, or is replacing, before the image providers run; the types
//! it saved are taken out of what those providers replace
//! (`UpdateReplaceImages`).

use std::sync::{Arc, Mutex};

use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemRepository, PeopleRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

use super::LibraryScanner;
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_type_lookup::{IdDerivation, derive_item_id};
use crate::people_repository::FerrofinPeopleRepository;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// Bytes that start like a PNG, which is all a download checks.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nIMAGE";

/// Bytes that start like a GIF: TheMovieDb's poster, told apart from the
/// NFO's PNG.
const GIF: &[u8] = b"GIF89aTMDB";

/// An image host on loopback: `/png/…` answers a PNG as `image/png`,
/// `/html/…` the same bytes as `text/html`, and `/img/…` (TheMovieDb's image
/// root) a GIF. Under `/heat` it is TheMovieDb's API for Heat (TMDB 949),
/// with one poster; everything else (TheMovieDb under `/tmdb`) 404. Returns
/// its base URL and every request line.
fn spawn_image_host() -> (String, Arc<Mutex<Vec<String>>>) {
    use std::io::{Read as _, Write as _};
    // Images are served on loopback, which downloads refuse by default.
    ferrofin_providers::destination::test_support::allow_loopback_images();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = request.lines().next().unwrap_or_default().to_owned();
            log.lock().expect("log").push(line.clone());
            let (status, content_type, body): (&str, &str, &[u8]) = if line.contains("/png/") {
                ("200 OK", "image/png", PNG)
            } else if line.contains("/html/") {
                ("200 OK", "text/html", PNG)
            } else if line.contains("/img/") {
                ("200 OK", "image/gif", GIF)
            } else if line.contains("/heat/search/movie") {
                (
                    "200 OK",
                    "application/json",
                    br#"{"results":[{"id":949,"title":"Heat","release_date":"1995-12-15"}]}"#,
                )
            } else if line.contains("/heat/movie/949/images") {
                (
                    "200 OK",
                    "application/json",
                    br#"{"posters":[{"file_path":"/tmdb-poster.gif","width":1000,"height":1500}],"backdrops":[],"logos":[]}"#,
                )
            } else if line.contains("/heat/movie/949") {
                (
                    "200 OK",
                    "application/json",
                    br#"{"id":949,"title":"Heat","release_date":"1995-12-15","videos":{"results":[]},"credits":{"cast":[],"crew":[]}}"#,
                )
            } else {
                ("404 Not Found", "application/json", b"{}")
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(body);
        }
    });
    (format!("http://{addr}"), requests)
}

/// One movie, `Heat (1995)/Heat (1995).mkv`, with its `Heat (1995).nfo`, in
/// an in-memory Movies library with default options (but for
/// [`saving_local_metadata`](Self::saving_local_metadata)), and the image
/// host.
struct Fixture {
    tmp: tempfile::TempDir,
    items: Arc<dyn ItemRepository>,
    people: Arc<FerrofinPeopleRepository>,
    vf: Arc<dyn VirtualFolderManager>,
    persistence: Arc<FerrofinItemPersistenceService>,
    host: String,
    /// Every request line the host answered.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    /// The library, its movie's NFO written by `nfo` from the host's URL.
    async fn new(nfo: impl FnOnce(&str) -> String) -> Self {
        Self::saving_local_metadata(false, nfo).await
    }

    /// [`new`](Self::new), with the library's "Save artwork into media
    /// folders" (`SaveLocalMetadata`) set to `save_local`.
    async fn saving_local_metadata(save_local: bool, nfo: impl FnOnce(&str) -> String) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, requests) = spawn_image_host();
        let media = tmp.path().join("movies");
        let file = media.join("Heat (1995)").join("Heat (1995).mkv");
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
        std::fs::write(&file, b"").expect("movie");
        std::fs::write(file.with_extension("nfo"), nfo(&host)).expect("nfo");
        let db = crate::test_support::test_db().await;
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
                save_local_metadata: save_local,
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("library");
        let items = crate::test_support::item_repository_over(db.clone());
        let people = Arc::new(
            FerrofinPeopleRepository::new(db).with_identity(
                IdDerivation::Jellyfin {
                    program_data_path: None,
                },
                tmp.path()
                    .join("metadata")
                    .join("People")
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        Self {
            tmp,
            items,
            people,
            vf,
            persistence,
            host,
            requests,
        }
    }

    /// How many requests to the host began with `prefix`.
    fn asked_prefix(&self, prefix: &str) -> usize {
        let request = format!("GET {prefix}");
        self.requests
            .lock()
            .expect("log")
            .iter()
            .filter(|line| line.starts_with(&request))
            .count()
    }

    /// How many times the host was asked for `path`.
    fn asked(&self, path: &str) -> usize {
        let request = format!("GET {path} ");
        self.requests
            .lock()
            .expect("log")
            .iter()
            .filter(|line| line.starts_with(&request))
            .count()
    }

    /// A scanner over the library, TheMovieDb's API on the host (where it
    /// finds nothing).
    fn scanner(&self) -> LibraryScanner {
        self.scanner_with(
            ferrofin_providers::TmdbClient::new().with_base_url(&format!("{}/tmdb", self.host)),
        )
    }

    /// A scanner over the library whose TheMovieDb knows Heat and its poster.
    fn scanner_knowing_heat(&self) -> LibraryScanner {
        self.scanner_with(
            ferrofin_providers::TmdbClient::new()
                .with_base_url(&format!("{}/heat", self.host))
                .with_image_root(&format!("{}/img", self.host)),
        )
    }

    /// A scanner over the library with `tmdb` as its TheMovieDb client.
    fn scanner_with(&self, tmdb: ferrofin_providers::TmdbClient) -> LibraryScanner {
        LibraryScanner::new(
            Arc::clone(&self.vf),
            Arc::new(FerrofinFileSystem::new()),
            self.persistence.clone(),
        )
        .with_items(Arc::clone(&self.items))
        .with_people(self.people.clone() as Arc<dyn PeopleRepository>)
        .with_metadata(
            Arc::new(tmdb),
            self.tmp.path().join("metadata").join("library"),
        )
    }

    /// The image saver the server wires: the metadata directory of the
    /// application paths over the temporary folder.
    async fn saver(&self) -> Arc<ferrofin_providers::image_save::ImageFileSaver> {
        let root = self.tmp.path();
        let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            root,
            root.join("log"),
            root.join("server-config"),
            root.join("cache"),
            root.join("web"),
        ));
        let configuration = Arc::new(
            crate::FerrofinServerConfigurationManager::load(paths)
                .await
                .expect("configuration"),
        );
        Arc::new(ferrofin_providers::image_save::ImageFileSaver::new(
            Arc::clone(&self.items),
            Arc::clone(&self.vf),
            configuration,
            root.join("metadata").join("library").into(),
        ))
    }

    /// The images of the person `name`.
    async fn person_images(&self, name: &str) -> Vec<ItemImageInfo> {
        let ids = self
            .people
            .find_person_item_ids(&[name.to_owned()])
            .await
            .expect("person ids");
        let Some(Some(id)) = ids.first().copied() else {
            panic!("no person {name}");
        };
        self.items.get_image_infos(id).await.expect("images")
    }

    /// The movie's folder.
    fn folder(&self) -> std::path::PathBuf {
        self.tmp.path().join("movies").join("Heat (1995)")
    }

    /// The movie's id.
    fn movie(&self) -> Uuid {
        let file = self.folder().join("Heat (1995).mkv");
        derive_item_id(BaseItemKind::Movie, &file.to_string_lossy()).expect("id")
    }

    /// The movie's images.
    async fn images(&self) -> Vec<ItemImageInfo> {
        self.items
            .get_image_infos(self.movie())
            .await
            .expect("images")
    }

    /// Dates the NFO an hour ahead, newer than the item's last save by more
    /// than the minute the reader allows, so the next scan reads it again.
    fn touch_nfo(&self) {
        std::fs::File::options()
            .write(true)
            .open(self.folder().join("Heat (1995).nfo"))
            .expect("nfo")
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_hours(1))
            .expect("mtime");
    }
}

/// The stored images' types and bytes.
fn stored(images: &[ItemImageInfo]) -> Vec<(ImageType, Vec<u8>)> {
    images
        .iter()
        .map(|image| {
            (
                image.image_type,
                std::fs::read(&image.path).expect("stored file"),
            )
        })
        .collect()
}

/// An NFO actor's `<thumb>` URL becomes the person's Primary image, asked
/// for once; an actor with none, or with a `<thumb>` naming a local file
/// (gl-1818), gets no image.
#[tokio::test(flavor = "multi_thread")]
async fn an_nfo_actors_thumb_becomes_their_photo() {
    let f = Fixture::new(|host| {
        format!(
            "<movie><title>Heat</title>\
             <actor><name>Ada Lovelace</name><thumb>{host}/png/ada.png</thumb></actor>\
             <actor><name>Bob Bit</name></actor>\
             <actor><name>Cy Path</name><thumb>/media/.actors/Cy_Path.png</thumb></actor>\
             </movie>"
        )
    })
    .await;

    Box::pin(f.scanner().scan_all()).await.expect("scan");

    assert_eq!(
        f.people
            .get_people_batch(&[f.movie()])
            .await
            .expect("cast")
            .remove(&f.movie())
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.name)
            .collect::<Vec<_>>(),
        ["Ada Lovelace", "Bob Bit", "Cy Path"],
        "the NFO's cast"
    );
    assert_eq!(
        stored(&f.person_images("Ada Lovelace").await),
        [(ImageType::Primary, PNG.to_vec())]
    );
    assert_eq!(f.asked("/png/ada.png"), 1);
    assert_eq!(f.person_images("Bob Bit").await, []);
    assert_eq!(f.person_images("Cy Path").await, []);
}

/// The NFO's poster and fanart URLs are saved as the movie's Primary and
/// Backdrop, with the art the scan downloads (the media folder is left
/// alone: the library does not save artwork into it).
#[tokio::test(flavor = "multi_thread")]
async fn an_nfos_poster_and_fanart_urls_become_the_items_artwork() {
    let f = Fixture::new(|host| {
        format!(
            "<movie><title>Heat</title><thumb aspect=\"poster\">{host}/png/poster.png</thumb>\
             <fanart><thumb>{host}/png/fanart.png</thumb></fanart></movie>"
        )
    })
    .await;

    Box::pin(f.scanner().scan_all()).await.expect("scan");

    let images = f.images().await;
    assert_eq!(
        stored(&images),
        [
            (ImageType::Primary, PNG.to_vec()),
            (ImageType::Backdrop, PNG.to_vec())
        ]
    );
    let metadata = f.tmp.path().join("metadata");
    assert!(
        images
            .iter()
            .all(|image| std::path::Path::new(&image.path).starts_with(&metadata)),
        "{images:?}"
    );
    let mut media: Vec<_> = std::fs::read_dir(f.folder())
        .expect("folder")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    media.sort();
    assert_eq!(media, ["Heat (1995).mkv", "Heat (1995).nfo"]);
    assert_eq!(
        (f.asked("/png/poster.png"), f.asked("/png/fanart.png")),
        (1, 1)
    );
}

/// A poster beside the movie is its Primary: the NFO's poster URL is never
/// asked for, and its fanart URL still fills the Backdrop.
#[tokio::test(flavor = "multi_thread")]
async fn a_poster_beside_the_movie_keeps_the_nfos_poster_url_from_being_asked() {
    let f = Fixture::new(|host| {
        format!(
            "<movie><title>Heat</title><thumb aspect=\"poster\">{host}/png/poster.png</thumb>\
             <fanart><thumb>{host}/png/fanart.png</thumb></fanart></movie>"
        )
    })
    .await;
    let sidecar = f.folder().join("poster.jpg");
    std::fs::write(&sidecar, b"\xFF\xD8\xFFsidecar").expect("sidecar");

    Box::pin(f.scanner().scan_all()).await.expect("scan");

    let images = f.images().await;
    assert_eq!(
        images
            .iter()
            .filter(|image| image.image_type == ImageType::Primary)
            .map(|image| image.path.as_str())
            .collect::<Vec<_>>(),
        [sidecar.to_string_lossy()]
    );
    assert_eq!(f.asked("/png/poster.png"), 0);
    assert_eq!(f.asked("/png/fanart.png"), 1);
}

/// Once the movie has the NFO's poster, a later read of the NFO does not
/// ask for it again.
#[tokio::test(flavor = "multi_thread")]
async fn an_nfo_url_is_not_asked_again_for_a_type_the_item_has() {
    let nfo = |host: &str, plot: &str| {
        format!(
            "<movie><title>Heat</title><plot>{plot}</plot>\
             <thumb aspect=\"poster\">{host}/png/poster.png</thumb></movie>"
        )
    };
    let f = Fixture::new(|host| nfo(host, "First.")).await;
    Box::pin(f.scanner().scan_all()).await.expect("scan");
    let first = f.images().await;
    std::fs::write(f.folder().join("Heat (1995).nfo"), nfo(&f.host, "Second.")).expect("nfo");
    f.touch_nfo();

    Box::pin(f.scanner().scan_all()).await.expect("rescan");

    let movie = f
        .items
        .retrieve_item(f.movie())
        .await
        .expect("read")
        .expect("movie");
    assert_eq!(
        movie.overview.as_deref(),
        Some("Second."),
        "the NFO was read again"
    );
    assert_eq!(f.asked("/png/poster.png"), 1);
    assert_eq!(
        stored(&f.images().await),
        [(ImageType::Primary, PNG.to_vec())]
    );
    assert_eq!(
        f.images()
            .await
            .into_iter()
            .map(|i| i.path)
            .collect::<Vec<_>>(),
        first.into_iter().map(|i| i.path).collect::<Vec<_>>()
    );
}

/// The NFO's poster stands against TheMovieDb's on a first scan, and
/// "Replace existing images" saves the NFO's again rather than TheMovieDb's:
/// the type the NFO filled is out of what the image providers replace.
#[tokio::test(flavor = "multi_thread")]
async fn replacing_images_keeps_the_nfos_poster_over_the_providers() {
    let f = Fixture::new(|host| {
        format!(
            "<movie><title>Heat</title><thumb aspect=\"poster\">{host}/png/poster.png</thumb></movie>"
        )
    })
    .await;
    Box::pin(f.scanner_knowing_heat().scan_all())
        .await
        .expect("scan");
    assert_eq!(
        stored(&f.images().await),
        [(ImageType::Primary, PNG.to_vec())],
        "the first scan"
    );
    let offered = f.asked_prefix("/heat/movie/949/images");
    assert!(offered > 0, "TheMovieDb offered its poster");

    let replace = MetadataRefreshOptions::for_item_refresh(
        MetadataRefreshMode::FullRefresh,
        MetadataRefreshMode::FullRefresh,
        false,
        true,
        false,
    );
    Box::pin(f.scanner_knowing_heat().scan_with(None, &replace))
        .await
        .expect("replace images");

    assert_eq!(
        stored(&f.images().await),
        [(ImageType::Primary, PNG.to_vec())]
    );
    assert_eq!(f.asked("/png/poster.png"), 2, "saved again");
    assert_eq!(
        f.asked_prefix("/heat/movie/949/images"),
        2 * offered,
        "TheMovieDb offered its poster again"
    );
    assert_eq!(f.asked_prefix("/img/"), 0, "and was never asked for it");
}

/// The control for the test above: a type the NFO names no URL for is still
/// filled by TheMovieDb, while the NFO's fanart is the Backdrop.
#[tokio::test(flavor = "multi_thread")]
async fn the_providers_still_fill_a_type_the_nfo_names_none_for() {
    let f = Fixture::new(|host| {
        format!(
            "<movie><title>Heat</title><fanart><thumb>{host}/png/fanart.png</thumb></fanart></movie>"
        )
    })
    .await;

    Box::pin(f.scanner_knowing_heat().scan_all())
        .await
        .expect("scan");

    let mut images = stored(&f.images().await);
    images.sort_by_key(|(image_type, _)| *image_type != ImageType::Primary);
    assert_eq!(
        images,
        [
            (ImageType::Primary, GIF.to_vec()),
            (ImageType::Backdrop, PNG.to_vec())
        ]
    );
}

/// An NFO URL that gives no image (a 404, a page that is no image, a host
/// that refuses the connection) is skipped, and fails no refresh, as
/// upstream catches its `HttpRequestException`: the movie is stamped
/// refreshed, where a failed provider request would leave it unstamped.
#[tokio::test(flavor = "multi_thread")]
async fn an_nfo_url_that_gives_no_image_is_skipped_and_fails_no_refresh() {
    ferrofin_providers::rate_limit::test_support::skip_backoff();
    // A loopback port nothing listens on: the connection is refused.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr");
    let f = Fixture::new(|host| {
        format!(
            "<movie><title>Heat</title><thumb aspect=\"poster\">{host}/html/poster.png</thumb>\
             <fanart><thumb>{host}/missing/fanart.png</thumb></fanart>\
             <thumb aspect=\"banner\">http://{closed}/banner.png</thumb></movie>"
        )
    })
    .await;

    Box::pin(f.scanner().scan_all()).await.expect("scan");

    assert_eq!(f.images().await, []);
    assert_eq!(
        (f.asked("/html/poster.png"), f.asked("/missing/fanart.png")),
        (1, 1)
    );
    let movie = f
        .items
        .retrieve_item(f.movie())
        .await
        .expect("read")
        .expect("movie");
    assert!(movie.date_last_refreshed.is_some(), "stamped");
}

/// The owner's rule (2026-10-17) for the NFO's artwork too: the image saver
/// puts it beside the movie only when the library saves artwork into media
/// folders, and otherwise leaves it in the server's metadata folder with the
/// media folder untouched.
#[rstest::rstest]
#[case::not_opted_in(false)]
#[case::opted_in(true)]
#[tokio::test(flavor = "multi_thread")]
async fn the_nfos_artwork_goes_into_the_media_folder_only_with_the_librarys_opt_in(
    #[case] save_local: bool,
) {
    let f = Fixture::saving_local_metadata(save_local, |host| {
        format!(
            "<movie><title>Heat</title><thumb aspect=\"poster\">{host}/png/poster.png</thumb></movie>"
        )
    })
    .await;

    Box::pin(f.scanner().with_image_saver(f.saver().await).scan_all())
        .await
        .expect("scan");

    let images = f.images().await;
    assert_eq!(stored(&images), [(ImageType::Primary, PNG.to_vec())]);
    let in_media = std::path::Path::new(&images[0].path).starts_with(f.folder());
    assert_eq!(in_media, save_local, "{images:?}");
    let media = std::fs::read_dir(f.folder()).expect("folder").count();
    assert_eq!(
        media,
        if save_local { 3 } else { 2 },
        "the mkv, the nfo, the poster"
    );
}
