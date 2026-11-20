//! What the scan does with a credited person's profile photo and a studio's
//! thumb it downloads ([`LibraryScanner::enrich_people`],
//! [`LibraryScanner::enrich_studio_images`]).
//!
//! Upstream saves both through `ProviderManager.SaveImage(item, url, …)`,
//! which refuses a response that is no image and names the file from the
//! type the response resolved to (gl-219), and hands it to `ImageSaver`,
//! which keeps a person's and a studio's artwork beside its own folder
//! (gl-787).

use std::sync::{Arc, Mutex};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_providers::image_save::ImageFileSaver;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, WrittenPerson};
use uuid::Uuid;

use super::{LibraryScanner, ScanRun};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
use crate::people_repository::FerrofinPeopleRepository;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// Bytes that start like a PNG, which is all a download checks.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nIMAGE";

/// A profile-photo and studio-artwork host on loopback: `/png/…` answers a
/// PNG as `image/png`, `/html/…` and Lenfilm's thumb the same bytes as
/// `text/html`, `/missing/…` 404, and `thumbs.txt` the studio manifest
/// `Mosfilm`, `Lenfilm`. Returns its base URL and every request line.
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
            let status = if line.contains("/missing/") {
                "404 Not Found"
            } else {
                "200 OK"
            };
            let (content_type, body): (&str, &[u8]) = if line.contains("thumbs.txt") {
                ("text/plain", b"Mosfilm\nLenfilm")
            } else if line.contains("/html/") || line.contains("/Lenfilm/") {
                ("text/html", PNG)
            } else {
                ("image/png", PNG)
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

/// A scanner over an in-memory database holding one `Person` and one
/// `Studio`, with the metadata directory in a temporary folder.
struct Fixture {
    tmp: tempfile::TempDir,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: Arc<dyn ItemRepository>,
    people: FerrofinPeopleRepository,
    person: Uuid,
    studio: Uuid,
    /// A studio whose thumb the host serves as `text/html`.
    refused_studio: Uuid,
    host: String,
    /// Every request line the host answered.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = crate::test_support::test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let metadata = tmp.path().join("metadata");
        let row = |kind, name: &str, path: std::path::PathBuf| BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(Uuid::new_v4()),
            type_: stored_type_name(kind).expect("type").to_owned(),
            name: Some(name.into()),
            path: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let person = row(
            BaseItemKind::Person,
            "Ada Lovelace",
            metadata.join("People").join("A").join("Ada Lovelace"),
        );
        let studio = row(
            BaseItemKind::Studio,
            "Mosfilm",
            metadata.join("Studio").join("Mosfilm"),
        );
        let refused_studio = row(
            BaseItemKind::Studio,
            "Lenfilm",
            metadata.join("Studio").join("Lenfilm"),
        );
        persistence
            .save_items(&[person.clone(), studio.clone(), refused_studio.clone()])
            .await
            .expect("seed");
        let (host, requests) = spawn_image_host();
        Self {
            persistence,
            items,
            people: FerrofinPeopleRepository::new(db),
            person: Uuid::parse_str(&person.id).expect("id"),
            studio: Uuid::parse_str(&studio.id).expect("id"),
            refused_studio: Uuid::parse_str(&refused_studio.id).expect("id"),
            host,
            requests,
            tmp,
        }
    }

    fn folders(&self) -> Arc<dyn VirtualFolderManager> {
        Arc::new(
            FerrofinVirtualFolderManager::new(self.tmp.path().join("root").join("default"))
                .with_item_store(self.persistence.clone()),
        )
    }

    fn scanner(&self) -> LibraryScanner {
        LibraryScanner::new(
            self.folders(),
            Arc::new(FerrofinFileSystem::new()),
            self.persistence.clone(),
        )
        .with_items(Arc::clone(&self.items))
        .with_metadata(
            Arc::new(ferrofin_providers::TmdbClient::new()),
            self.tmp.path().join("metadata").join("library"),
        )
        .with_studio_images(Arc::new(ferrofin_providers::StudiosClient::with_repo_url(
            &self.host,
        )))
    }

    /// The image saver the server wires: the metadata directory of the
    /// application paths over the temporary folder.
    async fn saver(&self) -> Arc<ImageFileSaver> {
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
        Arc::new(ImageFileSaver::new(
            Arc::clone(&self.items),
            self.folders(),
            configuration,
            root.join("metadata").join("library").into(),
        ))
    }

    /// The person credited with the profile photo at `path` on the host.
    fn credit(&self, path: &str) -> WrittenPerson {
        WrittenPerson {
            id: self.person,
            // No biography lookup.
            needs_details: false,
            image_url: Some(format!("{}{path}", self.host)),
            provider_id: None,
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

    async fn images(&self, id: Uuid) -> Vec<ItemImageInfo> {
        self.items.get_image_infos(id).await.expect("images")
    }
}

/// The stored image's file extension and bytes.
fn stored(images: &[ItemImageInfo], image_type: ImageType) -> Vec<(String, Vec<u8>)> {
    images
        .iter()
        .filter(|image| image.image_type == image_type)
        .map(|image| {
            let path = std::path::Path::new(&image.path);
            (
                path.extension()
                    .map_or_default(|ext| ext.to_string_lossy().into_owned()),
                std::fs::read(path).expect("stored file"),
            )
        })
        .collect()
}

/// gl-219: a PNG profile photo is stored as `.png`, not `.jpg`, and a
/// response the host calls `text/html` gives the person no photo.
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_photo_is_named_by_its_type_and_a_non_image_is_refused() {
    let f = Fixture::new().await;
    let scanner = f.scanner();

    scanner
        .enrich_people(&f.people, vec![f.credit("/html/ada.jpg")])
        .await;
    assert_eq!(f.images(f.person).await, [], "no image");

    scanner
        .enrich_people(&f.people, vec![f.credit("/png/ada.jpg")])
        .await;
    assert_eq!(
        stored(&f.images(f.person).await, ImageType::Primary),
        [("png".to_owned(), PNG.to_vec())]
    );
}

/// gl-219: a studio thumb is stored under the type it was served as, and a
/// thumb served as `text/html` is no thumb. That studio is still stamped
/// refreshed: the repository answered, so it is not asked again.
#[tokio::test(flavor = "multi_thread")]
async fn a_studio_thumb_is_named_by_its_type_and_a_non_image_is_refused() {
    let f = Fixture::new().await;

    f.scanner()
        .enrich_studio_images(ScanRun::defaults())
        .await
        .expect("pass");

    assert_eq!(
        stored(&f.images(f.studio).await, ImageType::Thumb),
        [("png".to_owned(), PNG.to_vec())]
    );
    assert_eq!(f.images(f.refused_studio).await, [], "no thumb");
    let refused = f
        .items
        .retrieve_item(f.refused_studio)
        .await
        .expect("read")
        .expect("row");
    assert!(refused.date_last_refreshed.is_some(), "answered");
}

/// gl-1477: a profile photo that cannot be downloaded is asked for once, not
/// again on every later save of an item crediting the person; a credit with
/// another URL still asks.
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_photo_that_failed_is_not_asked_for_again() {
    let f = Fixture::new().await;
    let scanner = f.scanner();

    for _ in 0..3 {
        scanner
            .enrich_people(&f.people, vec![f.credit("/missing/ada.jpg")])
            .await;
    }
    assert_eq!(f.asked("/missing/ada.jpg"), 1);
    assert_eq!(f.images(f.person).await, [], "no photo");

    scanner
        .enrich_people(&f.people, vec![f.credit("/png/ada.jpg")])
        .await;
    assert_eq!(
        stored(&f.images(f.person).await, ImageType::Primary),
        [("png".to_owned(), PNG.to_vec())],
        "another URL still asks"
    );
}

/// The files in `dir`, none when it does not exist.
fn files_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect()
}

/// gl-787: the photo and the thumb the scan downloads are saved where the
/// image saver keeps a person's and a studio's artwork, beside the item's
/// own folder (`ContainingFolderPath => Path`), as an upload is, and no copy
/// stays in the art dir.
#[tokio::test(flavor = "multi_thread")]
async fn a_downloaded_photo_and_thumb_are_saved_beside_the_item() {
    let f = Fixture::new().await;
    let scanner = f.scanner().with_image_saver(f.saver().await);

    scanner
        .enrich_people(&f.people, vec![f.credit("/png/ada.jpg")])
        .await;
    scanner
        .enrich_studio_images(ScanRun::defaults())
        .await
        .expect("pass");

    let metadata = f.tmp.path().join("metadata");
    for (id, image_type, published) in [
        (
            f.person,
            ImageType::Primary,
            metadata.join("People/A/Ada Lovelace/folder.png"),
        ),
        (
            f.studio,
            ImageType::Thumb,
            metadata.join("Studio/Mosfilm/landscape.png"),
        ),
    ] {
        let images: Vec<_> = f
            .images(id)
            .await
            .into_iter()
            .map(|image| (image.image_type, image.path))
            .collect();
        assert_eq!(
            images,
            [(image_type, published.to_string_lossy().into_owned())]
        );
        assert_eq!(std::fs::read(&published).expect("published"), PNG);
        let art_dir = crate::dynamic_images::own_art_dir(&metadata.join("library"), id);
        assert_eq!(files_in(&art_dir), Vec::<std::path::PathBuf>::new());
    }
}

/// gl-787 under the owner's rule that nothing is written into a media
/// library without its opt-in: a person whose folder is not in the server's
/// own directories (here, in a media folder) follows its library's "Save
/// artwork into media folders", and with no library opting in (here, none
/// holds the folder) the photo stays in the art dir and the folder is left
/// alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_is_not_written_into_a_media_folder_without_the_librarys_opt_in() {
    let f = Fixture::new().await;
    let folder = f.tmp.path().join("media").join("Ada Lovelace");
    let mut row = f
        .items
        .retrieve_item(f.person)
        .await
        .expect("read")
        .expect("row");
    row.path = Some(folder.to_string_lossy().into_owned());
    f.persistence.save_items(&[row]).await.expect("move");

    f.scanner()
        .with_image_saver(f.saver().await)
        .enrich_people(&f.people, vec![f.credit("/png/ada.jpg")])
        .await;

    let art_dir = crate::dynamic_images::own_art_dir(
        &f.tmp.path().join("metadata").join("library"),
        f.person,
    );
    let paths: Vec<_> = f
        .images(f.person)
        .await
        .into_iter()
        .map(|image| image.path)
        .collect();
    assert_eq!(
        paths,
        [art_dir.join("primary.png").to_string_lossy().into_owned()]
    );
    assert_eq!(files_in(&folder), Vec::<std::path::PathBuf>::new());
}

/// gl-219: the extension a download is stored under, from its type, when
/// the art-dir readers know it.
#[rstest::rstest]
#[case("image/jpeg", ".jpg")]
#[case("image/png", ".png")]
#[case("image/webp", ".webp")]
#[case("image/gif", ".gif")]
// Images the art-dir readers would not find again.
#[case("image/avif", ".jpg")]
#[case("image/bmp", ".jpg")]
#[case("image/x-unknown", ".jpg")]
fn a_download_takes_its_types_extension_when_the_art_dir_knows_it(
    #[case] content_type: &str,
    #[case] extension: &str,
) {
    assert_eq!(super::art_extension(content_type), extension);
}
