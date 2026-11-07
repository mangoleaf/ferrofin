//! Two saves of one item that overlap must both land (gl-142).
//!
//! Every test here queues its saves behind a held writer, which is what a
//! library scan does to them for seconds at a time. They run on a **file-backed**
//! database: an in-memory one shares a single pool between reads and writes, so a
//! held writer would stop the reads too and hide the window.

use std::time::Duration;

use super::*;
use crate::test_support::seed_user;

/// How long the saves get to run up to the writer. Only a save that reads
/// before it waits needs it, and such a save reads in well under this; a save
/// that reads in its write transaction passes however short it is.
const TIME_TO_REACH_THE_WRITER: Duration = Duration::from_millis(200);

/// How long a save that must not wait for the writer may take.
const NO_WAIT: Duration = Duration::from_secs(5);

/// A runtime the resume heuristics keep a position in.
const RUNTIME_TICKS: i64 = 72_000_000_000;
/// Half way in: a resume point, played neither way.
const POSITION_TICKS: i64 = RUNTIME_TICKS / 2;

const USER: Uuid = Uuid::from_u128(1);
const ITEM: Uuid = Uuid::from_u128(2);

/// A migrated database on disk, with the real reader pool and single writer,
/// holding [`USER`] and the movie [`ITEM`].
async fn file_db(dir: &tempfile::TempDir) -> Database {
    let url = format!("sqlite://{}", dir.path().join("user-data.db").display());
    let db = Database::connect(&url).await.expect("file connect");
    db.run_migrations().await.expect("migrations apply");
    seed_user(&db, USER).await;
    seed_movie_with_runtime(&db, ITEM, RUNTIME_TICKS).await;
    db
}

/// One user-data change, as a client request makes it.
#[derive(Debug, Clone, Copy)]
enum Save {
    Progress,
    Favourite,
    Like,
    PlayStart,
    MarkPlayed,
    MarkUnplayed,
}

impl Save {
    async fn run(self, mgr: &FerrofinUserDataManager) {
        match self {
            Self::Progress => {
                mgr.update_play_state(USER, ITEM, Some(POSITION_TICKS))
                    .await
                    .expect("progress report");
            }
            Self::Favourite => {
                mgr.save_user_data(USER, ITEM, &favorite_dto())
                    .await
                    .expect("favourite");
            }
            Self::Like => {
                mgr.set_likes(USER, ITEM, Some(true)).await.expect("like");
            }
            Self::PlayStart => {
                mgr.record_playback_start(USER, ITEM)
                    .await
                    .expect("play start");
            }
            Self::MarkPlayed => {
                mgr.mark_played(USER, ITEM, None)
                    .await
                    .expect("mark played");
            }
            Self::MarkUnplayed => {
                mgr.mark_unplayed(USER, ITEM).await.expect("mark unplayed");
            }
        }
    }

    /// Asserts this change is in `data`.
    fn assert_kept(self, data: &UserItemDataDto) {
        match self {
            Self::Progress => assert_eq!(data.playback_position_ticks, POSITION_TICKS),
            Self::Favourite => assert!(data.is_favorite, "the favourite was lost"),
            Self::Like => assert_eq!(data.likes, Some(true), "the like was lost"),
            Self::PlayStart => assert_eq!(data.play_count, 1, "the play start was lost"),
            Self::MarkPlayed => assert!(data.played, "the played mark was lost"),
            Self::MarkUnplayed => assert!(!data.played, "the unplayed mark was lost"),
        }
    }
}

/// Runs `saves` at once behind a held writer, releases it, and waits for them.
async fn behind_a_held_writer(db: &Database, mgr: &FerrofinUserDataManager, saves: &[Save]) {
    let held = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("hold the writer");
    let mut tasks = tokio::task::JoinSet::new();
    for &save in saves {
        let mgr = mgr.clone();
        tasks.spawn(async move { save.run(&mgr).await });
    }
    tokio::time::sleep(TIME_TO_REACH_THE_WRITER).await;
    held.rollback().await.expect("release the writer");
    while let Some(joined) = tasks.join_next().await {
        joined.expect("a save panicked");
    }
}

/// Saves of different fields, overlapping: each keeps the other's field, in
/// whichever order the writer takes them.
#[rstest::rstest]
#[case::progress_and_favourite(Save::Progress, Save::Favourite)]
#[case::progress_and_like(Save::Progress, Save::Like)]
#[case::play_start_and_favourite(Save::PlayStart, Save::Favourite)]
#[case::mark_played_and_favourite(Save::MarkPlayed, Save::Favourite)]
#[case::mark_unplayed_and_favourite(Save::MarkUnplayed, Save::Favourite)]
#[case::like_and_favourite(Save::Like, Save::Favourite)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_saves_of_one_item_both_land(#[case] first: Save, #[case] second: Save) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = FerrofinUserDataManager::new(db.clone(), config());
    if matches!(first, Save::MarkUnplayed) {
        Save::MarkPlayed.run(&mgr).await;
    }

    behind_a_held_writer(&db, &mgr, &[first, second]).await;

    let data = mgr
        .get_user_data_dto(ITEM, USER)
        .await
        .expect("read")
        .expect("user data");
    first.assert_kept(&data);
    second.assert_kept(&data);
}

/// Each of several overlapping play starts counts: a `PlayCount++` read before
/// the wait would count them once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_play_starts_all_count() {
    const STARTS: i32 = 8;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = FerrofinUserDataManager::new(db.clone(), config());

    behind_a_held_writer(&db, &mgr, &[Save::PlayStart; STARTS as usize]).await;

    let data = mgr
        .get_user_data_dto(ITEM, USER)
        .await
        .expect("read")
        .expect("user data");
    assert_eq!(data.play_count, STARTS);
}

/// A played mark reaches another version through that version's own save, which
/// keeps a favourite saved on it meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_played_mark_keeps_a_favourite_saved_on_another_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let (primary, copy) = version_pair(&db).await;
    let mgr = FerrofinUserDataManager::new(db.clone(), config());

    let held = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("hold the writer");
    let mark = {
        let mgr = mgr.clone();
        tokio::spawn(async move { mgr.mark_played(USER, primary, None).await })
    };
    let favourite = {
        let mgr = mgr.clone();
        tokio::spawn(async move { mgr.save_user_data(USER, copy, &favorite_dto()).await })
    };
    tokio::time::sleep(TIME_TO_REACH_THE_WRITER).await;
    held.rollback().await.expect("release the writer");
    mark.await.expect("join").expect("mark played");
    favourite.await.expect("join").expect("favourite");

    let data = mgr
        .get_user_data_dto(copy, USER)
        .await
        .expect("read")
        .expect("user data");
    assert!(data.played, "the played mark did not reach the version");
    assert!(data.is_favorite, "the favourite was lost");
}

/// A report that saves nothing (no position, the same tracks) reads and leaves:
/// it must not queue for the writer behind a scan to find that out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_that_saves_nothing_does_not_wait_for_the_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = FerrofinUserDataManager::new(db.clone(), config());
    let streams = ferrofin_traits::library::ReportedStreams {
        remember_audio: true,
        remember_subtitles: true,
        ..Default::default()
    };

    let held = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("hold the writer");
    let played = tokio::time::timeout(NO_WAIT, mgr.update_play_progress(USER, ITEM, None, streams))
        .await
        .expect("the report waited for the writer")
        .expect("report");
    held.rollback().await.expect("release the writer");

    assert!(!played);
}

/// A report that changes only the remembered tracks still saves, and keeps a
/// favourite saved while it waited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_track_change_keeps_a_favourite_saved_while_it_waited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = FerrofinUserDataManager::new(db.clone(), config());
    let streams = ferrofin_traits::library::ReportedStreams {
        audio_stream_index: Some(3),
        remember_audio: true,
        remember_subtitles: true,
        ..Default::default()
    };

    let held = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("hold the writer");
    let report = {
        let mgr = mgr.clone();
        tokio::spawn(async move { mgr.update_play_progress(USER, ITEM, None, streams).await })
    };
    let favourite = {
        let mgr = mgr.clone();
        tokio::spawn(async move { Save::Favourite.run(&mgr).await })
    };
    tokio::time::sleep(TIME_TO_REACH_THE_WRITER).await;
    held.rollback().await.expect("release the writer");
    report.await.expect("join").expect("report");
    favourite.await.expect("join");

    let (audio, _) = mgr.get_stream_default_data(ITEM, USER).await.expect("read");
    assert_eq!(audio, Some(3));
    Save::Favourite.assert_kept(
        &mgr.get_user_data_dto(ITEM, USER)
            .await
            .expect("read")
            .expect("user data"),
    );
}
