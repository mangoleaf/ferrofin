//! The artwork a Kodi NFO declares (gl-74): an `<actor><thumb>` photo.
//!
//! Upstream's NFO reader keeps an actor's `<thumb>` as the credit's
//! `PersonInfo.ImageUrl` (`XmlReaderExtensions.GetPersonFromXmlNode`), and
//! `LibraryManager.SavePeopleMetadataAsync` gives it to a person who has no
//! Primary image yet.

use std::sync::{Arc, Mutex};

use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemRepository, PeopleRepository};
use uuid::Uuid;

use super::LibraryScanner;
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_type_lookup::{IdDerivation, derive_item_id};
use crate::people_repository::FerrofinPeopleRepository;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// Bytes that start like a PNG, which is all a download checks.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nIMAGE";

/// An image host on loopback: `/png/…` answers a PNG as `image/png`, and
/// everything else (TheMovieDb's API under `/tmdb`) 404. Returns its base
/// URL and every request line.
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
/// an in-memory Movies library with default options, and the image host.
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
        LibraryScanner::new(
            Arc::clone(&self.vf),
            Arc::new(FerrofinFileSystem::new()),
            self.persistence.clone(),
        )
        .with_items(Arc::clone(&self.items))
        .with_people(self.people.clone() as Arc<dyn PeopleRepository>)
        .with_metadata(
            Arc::new(
                ferrofin_providers::TmdbClient::new().with_base_url(&format!("{}/tmdb", self.host)),
            ),
            self.tmp.path().join("metadata").join("library"),
        )
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

    /// The movie's id.
    fn movie(&self) -> Uuid {
        let file = self
            .tmp
            .path()
            .join("movies")
            .join("Heat (1995)")
            .join("Heat (1995).mkv");
        derive_item_id(BaseItemKind::Movie, &file.to_string_lossy()).expect("id")
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
