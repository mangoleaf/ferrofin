//! The library scan's external-stream discovery, end to end: a movie with
//! `Movie.eng.forced.srt` and `Movie.fra.srt` beside it must leave two
//! `IsExternal` subtitle rows in `MediaStreamInfos` — with the language and
//! flags the filenames declare, numbered before the embedded streams as
//! Jellyfin 10.11's `FFProbeVideoInfo.Fetch` numbers them — and a rescan must
//! keep exactly those rows (no duplicates of a sidecar the subtitle manager
//! had already recorded). The sidecar a read-only-library upload leaves in the
//! item's internal metadata folder is found too.
//!
//! Port target: `SubtitleResolver`/`AudioResolver` as run from
//! `FFProbeVideoInfo`, over the real scanner, repository and SQLite schema; the
//! ffprobe seam is a fake that answers by file extension.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinMediaStreamRepository,
    FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use uuid::Uuid;

/// The extension of `path`, with its dot (`Path.GetExtension`).
fn naming_ext(path: &str) -> &str {
    ferrofin_naming::path::extension(path)
}

/// `MediaStreamType` discriminants as `MediaStreamInfos.StreamType` stores them.
const AUDIO: i32 = 0;
const VIDEO: i32 = 1;
const SUBTITLE: i32 = 2;

/// An ffprobe stand-in that answers by extension: the movie file has a video
/// and an audio stream, a `.srt` a single `subrip` subtitle stream with no
/// language of its own. Records every probed path.
struct ProbeByExtension {
    probed: Mutex<Vec<(String, bool)>>,
}

impl ProbeByExtension {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            probed: Mutex::new(Vec::new()),
        })
    }

    fn probed(&self) -> Vec<(String, bool)> {
        self.probed.lock().map(|p| p.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl MediaEncoder for ProbeByExtension {
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
        request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let path = request.media_source.path.clone().unwrap_or_default();
        if let Ok(mut probed) = self.probed.lock() {
            probed.push((path.clone(), request.media_is_audio));
        }
        let streams = if naming_ext(&path).eq_ignore_ascii_case(".srt") {
            vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Subtitle,
                codec: Some("subrip".to_owned()),
                ..MediaStream::default()
            }]
        } else {
            vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("h264".to_owned()),
                    width: Some(1920),
                    height: Some(1080),
                    ..MediaStream::default()
                },
                MediaStream {
                    index: 1,
                    stream_type: MediaStreamType::Audio,
                    codec: Some("aac".to_owned()),
                    language: Some("eng".to_owned()),
                    channels: Some(6),
                    ..MediaStream::default()
                },
            ]
        };
        Ok(MediaSourceInfo {
            run_time_ticks: Some(30_000_000),
            media_streams: streams,
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(
        &self,
        _path: &str,
        _image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        unreachable!("no audio items in this library")
    }
    async fn extract_video_image(
        &self,
        _input_file: &str,
        _container: &str,
        _media_source: &MediaSourceInfo,
        _video_stream: &MediaStream,
        _threed_format: Option<Video3DFormat>,
        _offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        unreachable!("no frame extraction in a scan")
    }
    fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _ticks: i64) -> String {
        String::new()
    }
    async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// One movie with two subtitle sidecars, plus the neighbours the resolver
/// must ignore: a poster, an NFO, and a sidecar belonging to another title.
fn write_movie_library(media: &Path) -> std::path::PathBuf {
    let dir = media.join("Heat (1995)");
    std::fs::create_dir_all(&dir).expect("fixture dirs");
    std::fs::write(dir.join("Heat (1995).mkv"), b"").expect("media file");
    std::fs::write(dir.join("Heat (1995).eng.forced.srt"), b"1\n").expect("eng sidecar");
    std::fs::write(dir.join("Heat (1995).fra.srt"), b"1\n").expect("fra sidecar");
    std::fs::write(dir.join("Heat (1995).nfo"), b"<movie/>").expect("nfo");
    std::fs::write(dir.join("poster.jpg"), b"").expect("poster");
    std::fs::write(dir.join("Heat (1995) Trailer.srt"), b"1\n").expect("non-match");
    dir
}

struct Harness {
    db: Database,
    scanner: LibraryScanner,
    streams: Arc<FerrofinMediaStreamRepository>,
    probe: Arc<ProbeByExtension>,
    meta_root: std::path::PathBuf,
}

async fn harness(tmp: &Path) -> Harness {
    let media = tmp.join("movies");
    write_movie_library(&media);
    let meta_root = tmp.join("metadata").join("library");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
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

    let streams = Arc::new(FerrofinMediaStreamRepository::new(db.clone()));
    let probe = ProbeByExtension::new();
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_probe(
        Arc::clone(&probe) as Arc<dyn MediaEncoder>,
        Arc::clone(&streams) as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    )
    .with_metadata_dir(meta_root.clone());
    Harness {
        db,
        scanner,
        streams,
        probe,
        meta_root,
    }
}

async fn movie_id(db: &Database) -> Uuid {
    let id: String =
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#)
            .fetch_one(db.pool())
            .await
            .expect("the scanned movie row");
    Uuid::parse_str(&id).expect("movie id")
}

async fn streams_of(
    repo: &FerrofinMediaStreamRepository,
    item: Uuid,
) -> Vec<MediaStreamInfoEntity> {
    let mut rows = repo
        .get_media_streams(&MediaStreamQuery {
            item_id: item,
            stream_type: None,
            index: None,
        })
        .await
        .expect("stream rows");
    rows.sort_by_key(|r| r.stream_index);
    rows
}

fn file_name(row: &MediaStreamInfoEntity) -> &str {
    row.path
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_indexes_sidecar_subtitles_as_external_streams_and_rescans_idempotently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;

    h.scanner.scan_all().await.expect("scan");
    let movie = movie_id(&h.db).await;
    let rows = streams_of(&h.streams, movie).await;

    // Externals first (subtitles, in directory order), then the file's own
    // streams, all renumbered from 0.
    assert_eq!(rows.len(), 4, "{rows:?}");
    let indices: Vec<i64> = rows.iter().map(|r| r.stream_index).collect();
    assert_eq!(indices, vec![0, 1, 2, 3]);
    let types: Vec<i32> = rows.iter().map(|r| r.stream_type).collect();
    assert_eq!(types, vec![SUBTITLE, SUBTITLE, VIDEO, AUDIO]);

    let eng = rows
        .iter()
        .find(|r| file_name(r) == "Heat (1995).eng.forced.srt")
        .expect("the forced English sidecar is indexed");
    assert!(eng.is_external);
    assert_eq!(eng.language.as_deref(), Some("eng"));
    assert!(eng.is_forced, "`.forced` in the name sets IsForced");
    assert!(!eng.is_default);
    assert_eq!(eng.is_hearing_impaired, Some(false));
    assert_eq!(
        eng.codec.as_deref(),
        Some("subrip"),
        "the probe's codec is kept"
    );
    assert_eq!(eng.title, None, "no title token in the name");

    let fra = rows
        .iter()
        .find(|r| file_name(r) == "Heat (1995).fra.srt")
        .expect("the French sidecar is indexed");
    assert!(fra.is_external);
    assert_eq!(fra.language.as_deref(), Some("fra"));
    assert!(!fra.is_forced);

    // The embedded streams are untouched apart from their position.
    let video = &rows[2];
    assert!(!video.is_external);
    assert_eq!(video.codec.as_deref(), Some("h264"));
    assert_eq!(video.width, Some(1920));
    let audio = &rows[3];
    assert!(!audio.is_external);
    assert_eq!(audio.language.as_deref(), Some("eng"));

    // Only the movie and its two sidecars were probed: the NFO, poster and the
    // other title's sidecar never reached ffprobe, and a subtitle probe is a
    // non-audio probe (`MediaType = Subtitle`).
    let probed = h.probe.probed();
    let mut probed_names: Vec<&str> = probed
        .iter()
        .map(|(p, _)| {
            Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        })
        .collect();
    probed_names.sort_unstable();
    assert_eq!(
        probed_names,
        vec![
            "Heat (1995).eng.forced.srt",
            "Heat (1995).fra.srt",
            "Heat (1995).mkv"
        ]
    );
    assert!(probed.iter().all(|(_, is_audio)| !is_audio));

    // A downloaded sidecar the subtitle manager appended before this scan
    // (same file the resolver finds) must not survive as a duplicate: the
    // scan's stream save is a full replace.
    let mut with_duplicate = rows.clone();
    with_duplicate.push(MediaStreamInfoEntity {
        item_id: guid_to_db(movie),
        stream_index: 4,
        stream_type: SUBTITLE,
        is_external: true,
        path: fra.path.clone(),
        language: Some("fra".to_owned()),
        codec: Some("subrip".to_owned()),
        ..MediaStreamInfoEntity::default()
    });
    h.streams
        .save_media_streams(movie, &with_duplicate)
        .await
        .expect("simulate a download");
    assert_eq!(streams_of(&h.streams, movie).await.len(), 5);

    h.scanner.scan_all().await.expect("rescan");
    let again = streams_of(&h.streams, movie).await;
    assert_eq!(
        again.len(),
        4,
        "a rescan keeps exactly the two sidecars: {again:?}"
    );
    assert_eq!(
        again
            .iter()
            .filter(|r| r.is_external && r.stream_type == SUBTITLE)
            .count(),
        2
    );
    assert_eq!(
        again.iter().map(|r| r.stream_index).collect::<Vec<_>>(),
        indices
    );
    // The item id is stable across scans, so the rows stayed on the same movie.
    assert_eq!(movie_id(&h.db).await, movie);
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_finds_sidecars_in_the_items_internal_metadata_folder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    h.scanner.scan_all().await.expect("scan");
    let movie = movie_id(&h.db).await;
    assert_eq!(streams_of(&h.streams, movie).await.len(), 4);

    // An upload against a read-only library lands in
    // `{metadata}/library/{id2}/{id}` (the subtitle manager's fallback); the
    // next scan indexes it like a media-adjacent sidecar.
    let dashless = movie.simple().to_string();
    let internal = h.meta_root.join(&dashless[..2]).join(&dashless);
    std::fs::create_dir_all(&internal).expect("internal metadata dir");
    std::fs::write(internal.join("Heat (1995).deu.srt"), b"1\n").expect("uploaded sidecar");

    h.scanner.scan_all().await.expect("rescan");
    let rows = streams_of(&h.streams, movie).await;
    assert_eq!(rows.len(), 5, "{rows:?}");
    let deu = rows
        .iter()
        .find(|r| r.language.as_deref() == Some("deu"))
        .expect("the metadata-folder sidecar is indexed");
    assert!(deu.is_external);
    assert_eq!(
        deu.path.as_deref(),
        Some(
            internal
                .join("Heat (1995).deu.srt")
                .to_string_lossy()
                .as_ref()
        )
    );
    // Still contiguous, externals first.
    let types: Vec<i32> = rows.iter().map(|r| r.stream_type).collect();
    assert_eq!(types, vec![SUBTITLE, SUBTITLE, SUBTITLE, VIDEO, AUDIO]);
    assert_eq!(
        rows.iter().map(|r| r.stream_index).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
}
