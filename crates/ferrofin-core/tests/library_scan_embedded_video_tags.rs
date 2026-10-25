//! The container tags of a probed video, through a scan: v12.2
//! `FFProbeVideoInfo.FetchEmbeddedInfo` and `FetchPeople`. The prober writes
//! the item before the metadata providers run, so an NFO's answer replaces
//! what the tags gave and the tags fill what it left.

use std::sync::atomic::{AtomicBool, Ordering};
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
use ferrofin_model::data::PersonKind;
use ferrofin_model::dto::{BaseItemPerson, MediaSourceInfo};
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository, PeopleRepository as _};
use uuid::Uuid;

/// Probes every file as an H.264 video, carrying container tags while
/// `tagged` is on, with `people` as its tagged people.
struct Tagged {
    tagged: AtomicBool,
    people: Mutex<Vec<(&'static str, PersonKind)>>,
}

impl Tagged {
    fn new() -> Self {
        Self {
            tagged: AtomicBool::new(true),
            people: Mutex::new(vec![
                ("Dee Rector", PersonKind::Director),
                ("Wry Ter", PersonKind::Writer),
            ]),
        }
    }
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
            run_time_ticks: Some(60_000_000_000),
            container: Some("mp4".to_owned()),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Video,
                codec: Some("h264".to_owned()),
                width: Some(1920),
                height: Some(1080),
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        let media_source = self.get_media_info(request).await?;
        if !self.tagged.load(Ordering::SeqCst) {
            return Ok(MediaInfo {
                media_source,
                ..MediaInfo::default()
            });
        }
        let person = |&(name, kind): &(&str, PersonKind)| BaseItemPerson {
            name: Some(name.to_owned()),
            id: Uuid::nil(),
            role: None,
            type_: kind,
            primary_image_tag: None,
            image_blur_hashes: None,
        };
        Ok(MediaInfo {
            media_source,
            official_rating: Some("PG-13".to_owned()),
            genres: vec![" Drama ".to_owned(), "Thriller".to_owned()],
            studios: vec!["ACME Pictures".to_owned()],
            production_year: Some(2016),
            premiere_date: chrono::DateTime::parse_from_rfc3339("2016-05-04T00:00:00Z")
                .ok()
                .map(|date| date.with_timezone(&chrono::Utc)),
            overview: Some("From the file.".to_owned()),
            forced_sort_name: Some("Sorted By Tag".to_owned()),
            people: self.people.lock().unwrap().iter().map(person).collect(),
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
        // A first scan grabs a frame for a video with no artwork (the
        // Screen Grabber); a real ffmpeg delivers one.
        let frame = std::env::temp_dir().join(format!(
            "ferrofin-test-frame-{}.jpg",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&frame, b"FRAME").map_err(ServiceError::backend_source)?;
        Ok(frame.to_string_lossy().into_owned())
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

/// A one-movie library and its scanner.
struct Library {
    db: Database,
    people: Arc<FerrofinPeopleRepository>,
    encoder: Arc<Tagged>,
    scanner: LibraryScanner,
    tmp: tempfile::TempDir,
}

/// A one-movie library over `Movie (2020)/Movie (2020).mp4`, with `nfo`
/// beside it when given, scanned once.
async fn scanned(
    nfo: Option<&str>,
) -> (Database, Arc<FerrofinPeopleRepository>, tempfile::TempDir) {
    let library = library(nfo, true).await;
    library.scanner.scan_all().await.expect("scan");
    (library.db, library.people, library.tmp)
}

/// The library, not yet scanned, its file probed with tags when `tagged`.
async fn library(nfo: Option<&str>, tagged: bool) -> Library {
    let tmp = tempfile::tempdir().expect("tempdir");
    let movies = tmp.path().join("movies");
    let folder = movies.join("Movie (2020)");
    std::fs::create_dir_all(&folder).expect("fixture dirs");
    std::fs::write(folder.join("Movie (2020).mp4"), b"video").expect("media file");
    if let Some(nfo) = nfo {
        std::fs::write(folder.join("Movie (2020).nfo"), nfo).expect("nfo");
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
                path: movies.to_string_lossy().into_owned(),
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
    let people = Arc::new(FerrofinPeopleRepository::new(db.clone()));
    let encoder = Arc::new(Tagged::new());
    encoder.tagged.store(tagged, Ordering::SeqCst);
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
    Library {
        db,
        people,
        encoder,
        scanner,
        tmp,
    }
}

/// "Search for missing metadata" over the library, which probes again.
async fn search_for_missing(library: &Library) {
    use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
    library
        .scanner
        .scan_with(
            None,
            &MetadataRefreshOptions::for_item_refresh(
                MetadataRefreshMode::FullRefresh,
                MetadataRefreshMode::FullRefresh,
                false,
                false,
                false,
            ),
        )
        .await
        .expect("search for missing metadata");
}

/// The movie's stored columns the tags reach.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Row {
    #[sqlx(rename = "Id")]
    id: String,
    #[sqlx(rename = "OfficialRating")]
    official_rating: Option<String>,
    #[sqlx(rename = "Genres")]
    genres: Option<String>,
    #[sqlx(rename = "Studios")]
    studios: Option<String>,
    #[sqlx(rename = "ProductionYear")]
    production_year: Option<i64>,
    #[sqlx(rename = "PremiereDate")]
    premiere_date: Option<String>,
    #[sqlx(rename = "Overview")]
    overview: Option<String>,
    #[sqlx(rename = "ForcedSortName")]
    forced_sort_name: Option<String>,
    #[sqlx(rename = "SortName")]
    sort_name: Option<String>,
}

async fn movie(db: &Database) -> Row {
    sqlx::query_as(
        r#"SELECT "Id", "OfficialRating", "Genres", "Studios", "ProductionYear",
                  "PremiereDate", "Overview", "ForcedSortName", "SortName"
           FROM "BaseItems" WHERE "Type" LIKE '%.Movie'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("the movie was stored")
}

async fn credits(people: &FerrofinPeopleRepository, id: &str) -> Vec<(String, String)> {
    let id = Uuid::parse_str(id).expect("item id");
    let mut rows: Vec<_> = people
        .get_people_batch(&[id])
        .await
        .expect("people")
        .remove(&id)
        .unwrap_or_default()
        .into_iter()
        .map(|person| (person.name, person.person_type.unwrap_or_default()))
        .collect();
    rows.sort();
    rows
}

/// A new movie with no NFO takes its rating, genres (trimmed), studios,
/// premiere date, overview and forced sort name from the container, and its
/// credits from the tagged people.
#[tokio::test]
async fn a_new_movie_takes_its_container_tags() {
    let (db, people, _tmp) = scanned(None).await;
    let row = movie(&db).await;

    assert_eq!(row.official_rating.as_deref(), Some("PG-13"));
    assert_eq!(row.genres.as_deref(), Some("Drama|Thriller"));
    assert_eq!(row.studios.as_deref(), Some("ACME Pictures"));
    // The resolver's year from the folder name is set first, and the tag only
    // fills an empty one, as upstream's resolver and prober order them.
    assert_eq!(row.production_year, Some(2020));
    assert!(
        row.premiere_date
            .as_deref()
            .is_some_and(|date| date.starts_with("2016-05-04")),
        "{row:?}"
    );
    assert_eq!(row.overview.as_deref(), Some("From the file."));
    assert_eq!(row.forced_sort_name.as_deref(), Some("Sorted By Tag"));
    assert!(
        row.sort_name
            .as_deref()
            .is_some_and(|sort| sort.to_lowercase().starts_with("sorted by tag")),
        "the sort key follows the tagged forced sort name: {row:?}"
    );
    assert_eq!(
        credits(&people, &row.id).await,
        vec![
            ("Dee Rector".to_owned(), "Director".to_owned()),
            ("Wry Ter".to_owned(), "Writer".to_owned()),
        ]
    );
}

/// The NFO answers after the prober wrote the item, so its overview, genres
/// and cast replace the tags', and the tags fill what it left (the rating,
/// the studios).
#[tokio::test]
async fn an_nfo_replaces_the_container_tags_it_answers() {
    let nfo = "<movie><title>Movie</title><plot>From the NFO.</plot><genre>Comedy</genre>\
               <actor><name>Ann Actor</name><role>Lead</role></actor></movie>";
    let (db, people, _tmp) = scanned(Some(nfo)).await;
    let row = movie(&db).await;

    assert_eq!(row.overview.as_deref(), Some("From the NFO."));
    assert_eq!(row.genres.as_deref(), Some("Comedy"));
    assert_eq!(row.official_rating.as_deref(), Some("PG-13"));
    assert_eq!(row.studios.as_deref(), Some("ACME Pictures"));
    assert_eq!(
        credits(&people, &row.id).await,
        vec![("Ann Actor".to_owned(), "Actor".to_owned())]
    );
}

/// A stored movie the first scan probed without tags takes them, and the
/// tagged cast, when a later pass probes again ("Search for missing
/// metadata"): the prober writes the stored item, which has none of them.
#[tokio::test]
async fn a_stored_movie_takes_the_tags_it_lacks_when_probed_again() {
    let library = library(None, false).await;
    library.scanner.scan_all().await.expect("first scan");
    assert_eq!(movie(&library.db).await.official_rating, None);

    library.encoder.tagged.store(true, Ordering::SeqCst);
    search_for_missing(&library).await;

    let row = movie(&library.db).await;
    assert_eq!(row.official_rating.as_deref(), Some("PG-13"));
    assert_eq!(row.overview.as_deref(), Some("From the file."));
    assert_eq!(row.genres.as_deref(), Some("Drama|Thriller"));
    assert_eq!(row.studios.as_deref(), Some("ACME Pictures"));
    assert_eq!(row.forced_sort_name.as_deref(), Some("Sorted By Tag"));
    assert!(
        row.sort_name
            .as_deref()
            .is_some_and(|sort| sort.to_lowercase().starts_with("sorted by tag")),
        "{row:?}"
    );
    assert_eq!(
        credits(&library.people, &row.id).await,
        vec![
            ("Dee Rector".to_owned(), "Director".to_owned()),
            ("Wry Ter".to_owned(), "Writer".to_owned()),
        ]
    );
}

/// Without "Replace all metadata" a stored movie keeps its own values (an
/// edited overview) and its cast: the tags only fill, and `FetchPeople`
/// skips an item that already has people.
#[tokio::test]
async fn a_stored_movie_keeps_its_values_and_cast_when_probed_again() {
    let library = library(None, true).await;
    library.scanner.scan_all().await.expect("first scan");
    let id = movie(&library.db).await.id;
    sqlx::query(r#"UPDATE "BaseItems" SET "Overview" = 'Edited.' WHERE "Id" = ?1"#)
        .bind(&id)
        .execute(library.db.writer())
        .await
        .expect("edit the overview");
    *library.encoder.people.lock().unwrap() = vec![("New Person", PersonKind::Actor)];

    search_for_missing(&library).await;

    let row = movie(&library.db).await;
    assert_eq!(row.overview.as_deref(), Some("Edited."));
    assert_eq!(row.official_rating.as_deref(), Some("PG-13"));
    assert_eq!(
        credits(&library.people, &row.id).await,
        vec![
            ("Dee Rector".to_owned(), "Director".to_owned()),
            ("Wry Ter".to_owned(), "Writer".to_owned()),
        ]
    );
}
