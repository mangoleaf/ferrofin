//! `.strm` shortcuts through the real scanner: a library scan records the
//! shortcut and its remote target in the item's `Data` and never probes it; a
//! refresh with the remote content probe on probes the target (and not the
//! local file); a target that is not an `http`/`https`/`rtsp`/`rtp` URL is
//! never recorded. Playback's forced probe is the library manager's item
//! refresh, which probes the target and removes nothing. The ffprobe seam is a
//! fake that records its requests.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::media_source_manager::item_refresh_remote_probe;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemCountService, FerrofinItemPersistenceService,
    FerrofinItemRepository, FerrofinLibraryManager, FerrofinMediaStreamRepository,
    FerrofinPeopleRepository, FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::{MediaInfo, MediaProtocol};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// Answers every probe with one H.264 stream and records what it was asked.
#[derive(Default)]
struct Recording {
    requests: Mutex<Vec<(String, MediaProtocol)>>,
}

#[async_trait]
impl MediaEncoder for Recording {
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
        self.requests.lock().unwrap().push((
            request.media_source.path.clone().unwrap_or_default(),
            request.media_source.protocol,
        ));
        Ok(MediaSourceInfo {
            run_time_ticks: Some(6_000_000_000),
            container: Some("mp4".to_owned()),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Video,
                codec: Some("h264".to_owned()),
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
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
    scanner: Arc<LibraryScanner>,
    persistence: Arc<FerrofinItemPersistenceService>,
    encoder: Arc<Recording>,
    root: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

async fn harness(strms: &[(&str, &str)]) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let movies = tmp.path().join("movies");
    for (name, contents) in strms {
        let dir = movies.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.strm")), contents).unwrap();
    }
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
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
    .unwrap();
    let encoder = Arc::new(Recording::default());
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence.clone(),
    )
    .with_items(items)
    .with_probe(
        Arc::clone(&encoder) as Arc<dyn MediaEncoder>,
        Arc::new(FerrofinMediaStreamRepository::new(db.clone())) as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    );
    Harness {
        db,
        scanner: Arc::new(scanner),
        persistence,
        encoder,
        root: movies,
        _tmp: tmp,
    }
}

impl Harness {
    fn probed(&self) -> Vec<(String, MediaProtocol)> {
        self.encoder.requests.lock().unwrap().clone()
    }

    /// The stored `Data` of the movie named `name`, as JSON.
    async fn data(&self, name: &str) -> serde_json::Value {
        let data: Option<String> = sqlx::query_scalar(
            r#"SELECT "Data" FROM "BaseItems" WHERE "Path" LIKE ?1 AND "Type" LIKE '%Movie'"#,
        )
        .bind(format!("%{name}.strm"))
        .fetch_one(self.db.pool())
        .await
        .expect("the scanned movie");
        serde_json::from_str(&data.unwrap_or_else(|| "{}".to_owned())).unwrap()
    }

    async fn stream_count(&self, name: &str) -> i64 {
        sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "ItemId" =
               (SELECT "Id" FROM "BaseItems" WHERE "Path" LIKE ?1 AND "Type" LIKE '%Movie')"#,
        )
        .bind(format!("%{name}.strm"))
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }

    /// The `.strm` file of the movie named `name`.
    fn strm(&self, name: &str) -> String {
        self.root
            .join(name)
            .join(format!("{name}.strm"))
            .to_string_lossy()
            .into_owned()
    }

    /// How many movie rows the shortcut named `name` has.
    async fn movie_count(&self, name: &str) -> i64 {
        sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Path" LIKE ?1 AND "Type" LIKE '%Movie'"#,
        )
        .bind(format!("%{name}.strm"))
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }

    /// The library manager over this harness's scanner, as the server builds it.
    fn library(&self) -> Arc<dyn LibraryManager> {
        Arc::new(
            FerrofinLibraryManager::new(
                Arc::new(FerrofinItemRepository::new(
                    self.db.clone(),
                    Arc::new(ItemTypeLookup::new()),
                )),
                Arc::new(FerrofinItemCountService::new(self.db.clone())),
                self.persistence.clone(),
                Arc::new(FerrofinPeopleRepository::new(self.db.clone())),
            )
            .with_scanner(Arc::clone(&self.scanner)),
        )
    }

    async fn full_refresh(&self, name: &str, remote_probe: bool) {
        let path = self
            .root
            .join(name)
            .join(format!("{name}.strm"))
            .to_string_lossy()
            .into_owned();
        self.scanner
            .scan_paths_with(
                &[path],
                &MetadataRefreshOptions {
                    metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                    enable_remote_content_probe: remote_probe,
                    ..MetadataRefreshOptions::default()
                },
            )
            .await
            .unwrap();
    }
}

const REMOTE: &str = "# the movie\n\nhttp://origin.example/stream/movie.mp4\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_records_the_shortcut_and_its_remote_target_without_probing_it() {
    let h = harness(&[
        ("Remote (2020)", REMOTE),
        ("Local (2021)", "/etc/hostname\n"),
        ("Empty (2022)", "# nothing here\n"),
        ("Ftp (2023)", "ftp://origin.example/a.mp4\n"),
    ])
    .await;
    h.scanner.scan_all().await.unwrap();

    let remote = h.data("Remote (2020)").await;
    assert_eq!(remote["IsShortcut"], true);
    assert_eq!(
        remote["ShortcutPath"],
        "http://origin.example/stream/movie.mp4"
    );
    // A local path would let a `.strm` read any file the server can: never recorded.
    for name in ["Local (2021)", "Empty (2022)", "Ftp (2023)"] {
        let data = h.data(name).await;
        assert_eq!(data["IsShortcut"], true, "{name}");
        assert!(data.get("ShortcutPath").is_none(), "{name}: {data}");
    }
    assert!(
        h.probed().is_empty(),
        "a scan never probes a shortcut: {:?}",
        h.probed()
    );
    assert_eq!(h.stream_count("Remote (2020)").await, 0);

    // A second scan of unchanged files keeps what it recorded.
    h.scanner.scan_all().await.unwrap();
    assert_eq!(
        h.data("Remote (2020)").await["ShortcutPath"],
        "http://origin.example/stream/movie.mp4"
    );
    assert!(h.probed().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_content_probe_reads_the_target_not_the_file() {
    let h = harness(&[
        ("Remote (2020)", REMOTE),
        ("Local (2021)", "/etc/hostname\n"),
    ])
    .await;
    h.scanner.scan_all().await.unwrap();

    // Without the option a full refresh still probes nothing.
    h.full_refresh("Remote (2020)", false).await;
    assert!(h.probed().is_empty(), "{:?}", h.probed());

    h.full_refresh("Remote (2020)", true).await;
    assert_eq!(
        h.probed(),
        [(
            "http://origin.example/stream/movie.mp4".to_owned(),
            MediaProtocol::Http
        )]
    );
    assert_eq!(h.stream_count("Remote (2020)").await, 1);
    let remote = h.data("Remote (2020)").await;
    assert_eq!(
        remote["ShortcutPath"],
        "http://origin.example/stream/movie.mp4"
    );
    assert_eq!(remote["Container"], "mp4");

    // A shortcut with no remote target has nothing to probe, option or not.
    h.full_refresh("Local (2021)", true).await;
    assert_eq!(h.probed().len(), 1, "{:?}", h.probed());
    assert_eq!(h.stream_count("Local (2021)").await, 0);

    // The file changes its mind: the new target replaces the old one.
    std::fs::write(
        h.root.join("Remote (2020)/Remote (2020).strm"),
        "https://other.example/new.mkv\n",
    )
    .unwrap();
    h.full_refresh("Remote (2020)", true).await;
    assert_eq!(
        h.data("Remote (2020)").await["ShortcutPath"],
        "https://other.example/new.mkv"
    );
    assert_eq!(
        h.probed().last().unwrap().0,
        "https://other.example/new.mkv"
    );
    // …and one that no longer names a remote target leaves the last one in place.
    std::fs::write(
        h.root.join("Remote (2020)/Remote (2020).strm"),
        "/etc/passwd\n",
    )
    .unwrap();
    h.full_refresh("Remote (2020)", true).await;
    assert_eq!(
        h.data("Remote (2020)").await["ShortcutPath"],
        "https://other.example/new.mkv"
    );
}

/// Playback's forced probe (`GetPlaybackMediaSources`' `RefreshMetadata` of the
/// one item, the remote probe on) is the library manager's item refresh: it
/// probes the shortcut's remote target, and a shortcut whose file is gone stays
/// in the library, since an item refresh removes nothing. The path-scoped scan
/// the probe used to run beside the scan queue pruned it during playback.
#[tokio::test(flavor = "multi_thread")]
async fn playbacks_forced_probe_refreshes_the_item_and_removes_nothing() {
    let h = harness(&[("Remote (2020)", REMOTE), ("Gone (2021)", REMOTE)]).await;
    h.scanner.scan_all().await.unwrap();
    let library = h.library();
    let probe = item_refresh_remote_probe(Arc::downgrade(&library));

    probe(h.strm("Remote (2020)")).await.expect("probe");
    assert_eq!(
        h.probed(),
        [(
            "http://origin.example/stream/movie.mp4".to_owned(),
            MediaProtocol::Http
        )]
    );
    assert_eq!(h.stream_count("Remote (2020)").await, 1);

    let gone = h.strm("Gone (2021)");
    std::fs::remove_file(&gone).unwrap();
    probe(gone).await.expect("probe of a missing file");
    assert_eq!(
        h.movie_count("Gone (2021)").await,
        1,
        "the forced probe removed the item"
    );

    // Once the library manager is gone, the probe fails instead of scanning.
    drop(library);
    assert!(probe(h.strm("Remote (2020)")).await.is_err());
}
