//! `AudioFileProber.FetchDataFromTags`' people, over the real scanner: a track's
//! album artists, performers and composers become its credits; a changed tag
//! replaces them on the next probe; a `Cast` lock keeps what is stored. The
//! ffprobe seam is a fake that answers with raw tags.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::people_repository::FerrofinPeopleRepository;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaStreamRepository, FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository, PeopleRepository as _};
use uuid::Uuid;

/// Answers every probe with the tags it currently holds.
struct Tagged {
    tags: Mutex<HashMap<String, String>>,
    probes: std::sync::atomic::AtomicUsize,
    /// How many times the file has been "edited": each edit moves its mtime
    /// further than the last.
    edits: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl MediaEncoder for Tagged {
    fn encoder_path(&self) -> String {
        "ffmpeg".to_owned()
    }
    fn probe_path(&self) -> String {
        "ffprobe".to_owned()
    }
    async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_media_info(
        &self,
        _request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        Ok(MediaSourceInfo {
            run_time_ticks: Some(2_000_000_000),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Audio,
                codec: Some("mp3".to_owned()),
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
            raw_audio_tags: Some(self.tags.lock().unwrap().clone()),
            ..MediaInfo::default()
        })
    }
    async fn extract_audio_image(&self, _: &str, _: Option<i32>) -> Result<String, ServiceError> {
        Err(ServiceError::backend("no cover"))
    }
    async fn extract_video_image(
        &self,
        _: &str,
        _: &str,
        _: &MediaSourceInfo,
        _: &MediaStream,
        _: Option<Video3DFormat>,
        _: Option<i64>,
    ) -> Result<String, ServiceError> {
        unreachable!("no frames")
    }
    fn get_input_argument(&self, input_file: &str, _: &MediaSourceInfo) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _: i64) -> String {
        String::new()
    }
    async fn convert_image(&self, _: &str, _: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

struct Harness {
    db: Database,
    scanner: LibraryScanner,
    encoder: Arc<Tagged>,
    people: Arc<FerrofinPeopleRepository>,
    track: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let album = music.join("The Band").join("Album");
    std::fs::create_dir_all(&album).unwrap();
    let track = album.join("01 Song.mp3");
    std::fs::write(&track, b"").unwrap();
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Music",
        Some(CollectionTypeOptions::music),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: music.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .unwrap();
    let encoder = Arc::new(Tagged {
        tags: Mutex::new(HashMap::new()),
        probes: std::sync::atomic::AtomicUsize::new(0),
        edits: std::sync::atomic::AtomicU64::new(0),
    });
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let people = Arc::new(FerrofinPeopleRepository::new(db.clone()));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items)
    .with_probe(
        Arc::clone(&encoder) as Arc<dyn MediaEncoder>,
        Arc::new(FerrofinMediaStreamRepository::new(db.clone())) as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    )
    .with_people(Arc::clone(&people) as Arc<dyn ferrofin_traits::persistence::PeopleRepository>);
    Harness {
        db,
        scanner,
        encoder,
        people,
        track,
        _tmp: tmp,
    }
}

impl Harness {
    fn set_tags(&self, tags: &[(&str, &str)]) {
        *self.encoder.tags.lock().unwrap() = tags
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        // A changed file is probed again (and, as an editor's save does, its
        // folders' mtimes move with it).
        let edit = self
            .encoder
            .edits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600 * edit);
        for path in [
            &self.track,
            self.track.parent().unwrap(),
            self.track.parent().unwrap().parent().unwrap(),
        ] {
            std::fs::File::open(path)
                .unwrap()
                .set_modified(later)
                .unwrap();
        }
    }

    fn probes(&self) -> usize {
        self.encoder
            .probes
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn track_id(&self) -> Uuid {
        let id: String =
            sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%Audio.Audio'"#)
                .fetch_one(self.db.pool())
                .await
                .expect("the scanned track");
        Uuid::parse_str(&id).unwrap()
    }

    async fn credits(&self) -> Vec<(String, String)> {
        let id = self.track_id().await;
        let mut rows: Vec<_> = self
            .people
            .get_people_batch(&[id])
            .await
            .unwrap()
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .map(|person| (person.name, person.person_type.unwrap_or_default()))
            .collect();
        rows.sort();
        rows
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tracks_tags_become_its_credits_and_a_changed_tag_replaces_them() {
    let h = harness().await;
    let sep = '\u{1f}';
    h.set_tags(&[
        ("title", "Song"),
        ("artist", &format!("Lead{sep}Guest")),
        ("albumartist", "The Band"),
        ("composer", "Bach"),
    ]);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(
        h.credits().await,
        [
            ("Bach".to_owned(), "Composer".to_owned()),
            ("Guest".to_owned(), "Artist".to_owned()),
            ("Lead".to_owned(), "Artist".to_owned()),
            ("The Band".to_owned(), "AlbumArtist".to_owned()),
        ]
    );

    // A re-tag replaces the list rather than adding to it.
    h.set_tags(&[
        ("title", "Song"),
        ("artist", "Lead"),
        ("composer", "Handel"),
    ]);
    let outcome = h.scanner.scan_all().await.unwrap();
    let _ = outcome;
    assert_eq!(h.probes(), 2, "the changed file is probed again");
    assert_eq!(
        h.credits().await,
        [
            ("Handel".to_owned(), "Composer".to_owned()),
            ("Lead".to_owned(), "Artist".to_owned()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_lock_keeps_the_stored_credits() {
    let h = harness().await;
    h.set_tags(&[("title", "Song"), ("artist", "Lead")]);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(
        h.credits().await,
        [("Lead".to_owned(), "Artist".to_owned())]
    );

    // Lock Cast, then re-tag.
    let id = h.track_id().await;
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (?1, ?2)"#)
        .bind(ferrofin_db::enums::metadata_field::to_i32(
            ferrofin_model::entities::MetadataField::Cast,
        ))
        .bind(ferrofin_db::store::guid_to_db(id))
        .execute(h.db.pool())
        .await
        .unwrap();
    h.set_tags(&[("title", "Song"), ("artist", "Someone Else")]);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(
        h.credits().await,
        [("Lead".to_owned(), "Artist".to_owned())]
    );
}
