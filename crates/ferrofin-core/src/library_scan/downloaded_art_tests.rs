//! What the scan does with a credited person's profile photo and a studio's
//! thumb it downloads ([`LibraryScanner::enrich_people`],
//! [`LibraryScanner::enrich_studio_images`]).
//!
//! Upstream saves both through `ProviderManager.SaveImage(item, url, …)`,
//! which refuses a response that is no image and names the file from the
//! type the response resolved to (gl-219).

use std::sync::Arc;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
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
/// `text/html`, and `thumbs.txt` the studio manifest `Mosfilm`, `Lenfilm`.
/// Returns its base URL.
fn spawn_image_host() -> String {
    use std::io::{Read as _, Write as _};
    // Images are served on loopback, which downloads refuse by default.
    ferrofin_providers::destination::test_support::allow_loopback_images();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = request.lines().next().unwrap_or_default().to_owned();
            let (content_type, body): (&str, &[u8]) = if line.contains("thumbs.txt") {
                ("text/plain", b"Mosfilm\nLenfilm")
            } else if line.contains("/html/") || line.contains("/Lenfilm/") {
                ("text/html", PNG)
            } else {
                ("image/png", PNG)
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(body);
        }
    });
    format!("http://{addr}")
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
        Self {
            persistence,
            items,
            people: FerrofinPeopleRepository::new(db),
            person: Uuid::parse_str(&person.id).expect("id"),
            studio: Uuid::parse_str(&studio.id).expect("id"),
            refused_studio: Uuid::parse_str(&refused_studio.id).expect("id"),
            host: spawn_image_host(),
            tmp,
        }
    }

    fn scanner(&self) -> LibraryScanner {
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(self.tmp.path().join("root").join("default"))
                .with_item_store(self.persistence.clone()),
        );
        LibraryScanner::new(
            vf,
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
                    .map(|ext| ext.to_string_lossy().into_owned())
                    .unwrap_or_default(),
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
