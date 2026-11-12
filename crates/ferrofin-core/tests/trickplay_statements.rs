//! A trickplay refresh reads a video's `TrickplayInfos` rows once.
//!
//! Every video goes through the trickplay manager on each library scan and
//! each Generate Trickplay Images run, most of them with nothing to do. The
//! refresh used to re-read the video's rows in each of its steps (the orphan
//! prune, discovery, each width's existing-tiles check and the folder prune,
//! and on a checking pass the chroma check, the dropped-width removal and the
//! final match), four reads per unchanged video and seven on a checking pass
//! (gl-1266). It now reads them once and keeps them up to date as it writes.
//!
//! Generate Trickplay Images also read each video's row and streams again,
//! although its page already holds the row; it now hands each video its page
//! row and the page's streams, read once (gl-1375).
//!
//! This binary records the statements one call runs ([`sql_statements`]).

mod sql_statements;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::scheduled_tasks::library::TrickplayImagesTask;
use ferrofin_core::{
    FerrofinItemCountService, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinLibraryManager, FerrofinMediaStreamRepository, FerrofinPathManager,
    FerrofinPeopleRepository, FerrofinServerApplicationPaths, FerrofinTrickplayManager,
    FerrofinVirtualFolderManager, ItemTypeLookup, ScheduledTask, TaskProgress,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, MediaStreamInfoEntity};
use ferrofin_db::entities::playback::TrickplayInfoEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{
    LibraryOptions, MediaPathInfo, ServerConfiguration, TrickplayOptions,
};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayFrameExtractor};
use ferrofin_traits::persistence::{ItemPersistenceService, MediaStreamRepository};
use ferrofin_traits::system::{MediaLocation, PathManager, ServerApplicationPaths};
use ferrofin_traits::trickplay::TrickplayManager;
use sql_statements::{record_statements, statements_of};
use uuid::Uuid;

/// The trickplay settings a test sets.
struct FixedConfig {
    options: Mutex<TrickplayOptions>,
    paths: Arc<dyn ServerApplicationPaths>,
}

#[async_trait]
impl ServerConfigurationManager for FixedConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::clone(&self.paths)
    }

    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(ServerConfiguration {
            trickplay_options: self.options.lock().expect("lock").clone(),
            ..ServerConfiguration::default()
        }))
    }

    async fn update_configuration(&self, _: &ServerConfiguration) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
        Ok(ferrofin_model::branding::BrandingOptions::default())
    }

    async fn update_branding(
        &self,
        _: &ferrofin_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// An extractor no test here reaches: every video already has its set.
struct NoExtraction;

#[async_trait]
impl TrickplayFrameExtractor for NoExtraction {
    async fn extract_trickplay_frames(
        &self,
        _: &TrickplayExtraction<'_>,
    ) -> Result<Vec<String>, ServiceError> {
        Err(ServiceError::backend("no extraction expected"))
    }
}

/// One width, a 2x2 grid.
fn options(full_chroma: bool) -> TrickplayOptions {
    TrickplayOptions {
        width_resolutions: vec![320],
        tile_width: 2,
        tile_height: 2,
        interval: 10_000,
        full_chroma,
        ..TrickplayOptions::default()
    }
}

/// A manager over `db`, and a movie with a stored video stream, a media file
/// and its 320-wide set already made (row and tiles).
async fn movie_with_its_set(
    db: &Database,
    root: &std::path::Path,
) -> (FerrofinTrickplayManager, Arc<FixedConfig>, Uuid) {
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root.join("program"),
        root.join("log"),
        root.join("config"),
        root.join("cache"),
        root.join("web"),
    ));
    let path_manager: Arc<dyn PathManager> = Arc::new(FerrofinPathManager::new(paths.clone()));
    let config = Arc::new(FixedConfig {
        options: Mutex::new(options(false)),
        paths,
    });
    let streams = Arc::new(FerrofinMediaStreamRepository::new(db.clone()));
    let manager = FerrofinTrickplayManager::new(
        db.clone(),
        Arc::clone(&path_manager),
        Arc::clone(&config) as Arc<dyn ServerConfigurationManager>,
        Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        Arc::clone(&streams) as Arc<dyn MediaStreamRepository>,
        Arc::new(NoExtraction),
        Arc::new(ferrofin_drawing::ImageCrateEncoder::new()),
    );

    let id = Uuid::new_v4();
    let media = root.join("media").join("movie.mkv");
    std::fs::create_dir_all(media.parent().expect("parent")).expect("media dir");
    std::fs::write(&media, b"not really a video").expect("media file");
    let media = media.to_string_lossy().into_owned();
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[BaseItemEntity {
            id: guid_to_db(id),
            type_: BaseItemKind::Movie
                .stored_type_name()
                .expect("a stored type")
                .to_owned(),
            path: Some(media.clone()),
            run_time_ticks: Some(3_600 * 10_000_000),
            media_type: Some("Video".to_owned()),
            ..BaseItemEntity::default()
        }])
        .await
        .expect("save movie");
    streams
        .save_media_streams(
            id,
            &[MediaStreamInfoEntity {
                item_id: guid_to_db(id),
                stream_index: 0,
                stream_type: MediaStreamType::Video.json_value(),
                codec: Some("h264".to_owned()),
                width: Some(1920),
                ..MediaStreamInfoEntity::default()
            }],
        )
        .await
        .expect("save stream");

    let set = std::path::Path::new(&path_manager.trickplay_directory(
        id,
        MediaLocation::of_file(&media),
        false,
    ))
    .join("320 - 2x2");
    std::fs::create_dir_all(&set).expect("set folder");
    std::fs::write(set.join("0.jpg"), b"a tile").expect("tile");
    manager
        .save_trickplay_info(&TrickplayInfoEntity {
            item_id: guid_to_db(id),
            width: 320,
            height: 180,
            tile_width: 2,
            tile_height: 2,
            thumbnail_count: 4,
            interval: 10_000,
            bandwidth: 1_000,
        })
        .await
        .expect("save row");
    (manager, config, id)
}

fn extraction_on() -> LibraryOptions {
    LibraryOptions {
        enable_trickplay_image_extraction: true,
        ..LibraryOptions::default()
    }
}

/// How many statements read `TrickplayInfos`.
fn row_reads(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|s| s.contains("SELECT") && s.contains(r#"FROM "TrickplayInfos""#))
        .count()
}

/// The widths stored for `id`.
async fn stored_widths(manager: &FerrofinTrickplayManager, id: Uuid) -> Vec<i32> {
    let mut widths: Vec<i32> = manager
        .get_trickplay_resolutions(id)
        .await
        .expect("rows")
        .into_keys()
        .collect();
    widths.sort_unstable();
    widths
}

// A multi-thread runtime, so the folder probes run through `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_video_reads_its_rows_once() {
    record_statements();
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let root = tempfile::tempdir().expect("tempdir");
    let (manager, _, id) = movie_with_its_set(&db, root.path()).await;

    let (refreshed, statements) =
        statements_of(manager.refresh_trickplay_data(id, false, &extraction_on())).await;

    refreshed.expect("refresh");
    assert_eq!(stored_widths(&manager, id).await, [320], "the set is kept");
    assert_eq!(
        row_reads(&statements),
        1,
        "the rows are read once: {statements:#?}"
    );
}

// A multi-thread runtime, so the folder probes run through `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn a_checking_pass_reads_a_videos_rows_once() {
    record_statements();
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let root = tempfile::tempdir().expect("tempdir");
    let (manager, config, id) = movie_with_its_set(&db, root.path()).await;
    manager
        .record_trickplay_settings_once()
        .await
        .expect("record settings");
    // A chroma change makes the next run a checking pass; the tile has no
    // readable chroma, so the set is kept as it is.
    *config.options.lock().expect("lock") = options(true);
    let check = manager
        .begin_trickplay_pass()
        .await
        .expect("begin")
        .expect("a checking pass");

    let (matched, statements) =
        statements_of(manager.check_trickplay_data(id, &check, &extraction_on())).await;

    assert!(matched.expect("check"), "the set matches");
    assert_eq!(stored_widths(&manager, id).await, [320], "the set is kept");
    assert_eq!(
        row_reads(&statements),
        1,
        "the rows are read once: {statements:#?}"
    );
}

/// How many statements read `MediaStreamInfos`.
fn stream_reads(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|s| s.contains("SELECT") && s.contains(r#"FROM "MediaStreamInfos""#))
        .count()
}

/// How many statements read one `BaseItems` row by id
/// (`ItemRepository::retrieve_item`).
fn row_by_id_reads(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|s| s.contains(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#))
        .count()
}

// Generate Trickplay Images hands each video its page row and the page's
// streams, read once, on a generating and on a checking pass.
#[rstest::rstest]
#[case::generating(false)]
#[case::checking(true)]
// A multi-thread runtime, so the folder probes run through `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn the_task_reads_a_pages_streams_once_and_no_row_again(#[case] checking: bool) {
    record_statements();
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let root = tempfile::tempdir().expect("tempdir");
    let (manager, config, id) = movie_with_its_set(&db, root.path()).await;
    // A second video, not probed yet: without streams it has nothing to make.
    let unprobed = Uuid::new_v4();
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    persistence
        .save_items(&[BaseItemEntity {
            id: guid_to_db(unprobed),
            type_: BaseItemKind::Movie
                .stored_type_name()
                .expect("a stored type")
                .to_owned(),
            path: Some(
                root.path()
                    .join("media/unprobed.mkv")
                    .to_string_lossy()
                    .into_owned(),
            ),
            media_type: Some("Video".to_owned()),
            ..BaseItemEntity::default()
        }])
        .await
        .expect("save the second movie");
    if checking {
        manager
            .record_trickplay_settings_once()
            .await
            .expect("record settings");
        *config.options.lock().expect("lock") = options(true);
    }
    let folders = Arc::new(
        FerrofinVirtualFolderManager::new(root.path().join("libraries"))
            .with_item_store(persistence.clone()),
    );
    folders
        .add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: root.path().join("media").to_string_lossy().into_owned(),
                }],
                ..extraction_on()
            },
        )
        .await
        .expect("library");
    let library = Arc::new(FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        persistence,
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    ));
    let manager = Arc::new(manager);
    let task = TrickplayImagesTask::new(
        library,
        folders,
        manager.clone(),
        Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
    );

    let (ran, statements) = statements_of(task.execute(&TaskProgress::default())).await;

    ran.expect("run");
    assert_eq!(stored_widths(&manager, id).await, [320], "the set is kept");
    assert_eq!(
        stream_reads(&statements),
        1,
        "the page's streams are read once: {statements:#?}"
    );
    assert_eq!(
        row_by_id_reads(&statements),
        0,
        "no video's row is read again: {statements:#?}"
    );
    // The probed video's streams reach the manager, which reads its rows
    // once; the unprobed one is skipped before any.
    assert_eq!(
        row_reads(&statements),
        1,
        "one video's trickplay rows are read: {statements:#?}"
    );
}
