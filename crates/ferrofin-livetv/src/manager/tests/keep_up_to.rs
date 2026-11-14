//! A series timer's "Keep up to": `RecordingsManager.EnforceKeepUpTo`, which
//! runs as each of the series timer's recordings starts and deletes its oldest
//! recordings past the limit.

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::live_tv::{
    BaseTimerInfoDto, LiveTvOptions, RecordingQuery, RecordingStatus, SeriesTimerInfoDto,
    TimerInfoDto,
};
use ferrofin_traits::stubs::LiveTvManager;
use uuid::Uuid;

use super::recording_folders::{FakeLibrary, manager};
use super::{manager_with_dvr, wait_for};
use crate::dvr::TimerRecordingInfo;
use crate::manager::FerrofinLiveTvManager;

const SERIES_TIMER: &str = "5eb1e5715eb1e5715eb1e5715eb1e571";

/// Stores the series timer, keeping `keep_up_to` recordings.
async fn series_timer(mgr: &FerrofinLiveTvManager, keep_up_to: i32) {
    let timer = SeriesTimerInfoDto {
        base: BaseTimerInfoDto {
            id: Some(SERIES_TIMER.to_owned()),
            name: Some("Show".to_owned()),
            ..BaseTimerInfoDto::default()
        },
        keep_up_to,
        ..SeriesTimerInfoDto::default()
    };
    crate::dvr_repository::upsert_series_timer(&mgr.db, &timer, SERIES_TIMER, None)
        .await
        .expect("series timer");
}

/// A timer of `series_timer_id` named `id`, ending `minutes_ago`.
fn timer(
    id: &str,
    series_timer_id: &str,
    minutes_ago: i64,
    status: RecordingStatus,
) -> TimerInfoDto {
    let end = Utc::now() - Duration::minutes(minutes_ago);
    TimerInfoDto {
        base: BaseTimerInfoDto {
            id: Some(id.to_owned()),
            name: Some("Show".to_owned()),
            start_date: end - Duration::minutes(30),
            end_date: end,
            ..BaseTimerInfoDto::default()
        },
        status,
        series_timer_id: Some(series_timer_id.to_owned()),
        ..TimerInfoDto::default()
    }
}

/// A finished recording of `series_timer_id` in `folder`, as `settle_timer`
/// leaves one: the timer `Completed`, its recording row naming the file. The
/// file is written only when `written`.
async fn completed(
    mgr: &FerrofinLiveTvManager,
    folder: &Path,
    id: &str,
    series_timer_id: &str,
    minutes_ago: i64,
    written: bool,
) -> PathBuf {
    let timer = timer(id, series_timer_id, minutes_ago, RecordingStatus::Completed);
    mgr.persist_timer(&timer).await.expect("timer");
    let path = folder.join(format!("{id}.ts"));
    if written {
        std::fs::write(&path, b"recorded").expect("recording");
    }
    let recording = Uuid::new_v4();
    let info = TimerRecordingInfo::from_timer(&timer);
    crate::dvr_repository::insert_recording(&mgr.db, recording, &info, &path, timer.base.end_date)
        .await
        .expect("recording");
    crate::dvr_repository::finish_recording(
        &mgr.db,
        recording,
        RecordingStatus::Completed,
        Some(path.display().to_string().as_str()),
    )
    .await
    .expect("finished");
    path
}

/// The paths every recording row names.
async fn recorded_paths(mgr: &FerrofinLiveTvManager) -> Vec<String> {
    crate::dvr_repository::recording_rows(&mgr.db, &RecordingQuery::default())
        .await
        .expect("rows")
        .into_iter()
        .filter_map(|row| row.path)
        .collect()
}

/// The start of a series timer's recording deletes its oldest finished
/// recordings past "Keep up to", counting the one starting: their files, their
/// rows and their timers. A finished recording whose file has gone is not
/// counted, and another series timer's recordings and the capture itself are
/// left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_starting_recording_deletes_the_oldest_past_keep_up_to() {
    let dir = tempfile::tempdir().expect("directory");
    let mgr = manager_with_dvr(dir.path()).await;
    let series = dir.path().join("Series").join("Show");
    let season = series.join("Season 1");
    std::fs::create_dir_all(&season).expect("season");
    series_timer(&mgr, 2).await;

    let gone = completed(&mgr, &season, "gone", SERIES_TIMER, 10, false).await;
    let recent = completed(&mgr, &season, "recent", SERIES_TIMER, 60, true).await;
    let older = completed(&mgr, &season, "older", SERIES_TIMER, 120, true).await;
    let oldest = completed(&mgr, &season, "oldest", SERIES_TIMER, 180, true).await;
    let other = completed(&mgr, &season, "other", "another-series-timer", 240, true).await;

    let mut active = timer("active", SERIES_TIMER, -60, RecordingStatus::New);
    mgr.persist_timer(&active).await.expect("timer");
    let info = TimerRecordingInfo::from_timer(&active);
    let target = season.join("active.ts");
    std::fs::write(&target, b"recording").expect("capture");
    let options = LiveTvOptions {
        save_recording_nfo: false,
        ..LiveTvOptions::default()
    };
    mgr.start_capture(
        &mut active,
        &info,
        &target,
        &MediaSourceInfo::default(),
        (Some(&series), &options),
    )
    .await
    .expect("started");

    let deleted = wait_for(|| async {
        let timers_gone = mgr.get_timer("older").await.expect("read").is_none()
            && mgr.get_timer("oldest").await.expect("read").is_none();
        (timers_gone && !older.exists() && !oldest.exists()).then_some(())
    })
    .await;
    assert!(deleted.is_some(), "the recordings past the limit go");

    for kept in [&recent, &other, &target] {
        assert!(kept.exists(), "{} is kept", kept.display());
    }
    for id in ["gone", "recent", "other", "active"] {
        assert!(mgr.get_timer(id).await.expect("read").is_some(), "{id}");
    }
    let mut paths = recorded_paths(&mgr).await;
    paths.sort();
    let mut expected = [&gone, &recent, &other, &target].map(|p| p.display().to_string());
    expected.sort();
    assert_eq!(paths, expected);
}

/// `OrderByDescending(EndDate).Skip(KeepUpTo - 1)` over the series timer's
/// finished recordings whose file is there; nothing at all for a limit of 0
/// or below.
#[rstest::rstest]
#[case::no_limit(0, &[])]
#[case::negative(i32::MIN, &[])]
#[case::only_the_one_starting(1, &["older", "oldest", "recent"])]
#[case::one_more(2, &["older", "oldest"])]
#[case::two_more(3, &["oldest"])]
#[case::all_within(4, &[])]
#[tokio::test]
async fn keep_up_to_deletes_the_oldest_past_the_limit(
    #[case] keep_up_to: i32,
    #[case] expected: &[&str],
) {
    let dir = tempfile::tempdir().expect("directory");
    let mgr = manager_with_dvr(dir.path()).await;
    let series = dir.path().join("Series").join("Show");
    std::fs::create_dir_all(&series).expect("series");
    series_timer(&mgr, keep_up_to).await;
    let mut files = Vec::new();
    // Upstream matches the series timer id ignoring case.
    let upper = SERIES_TIMER.to_uppercase();
    for (id, series_timer_id, minutes_ago, written) in [
        ("gone", SERIES_TIMER, 10, false),
        ("recent", SERIES_TIMER, 60, true),
        ("older", upper.as_str(), 120, true),
        ("oldest", SERIES_TIMER, 180, true),
    ] {
        let path = completed(&mgr, &series, id, series_timer_id, minutes_ago, written).await;
        files.push((id, path));
    }

    mgr.enforce_keep_up_to(Some(SERIES_TIMER), Some(&series))
        .await
        .expect("enforced");

    let mut deleted: Vec<&str> = Vec::new();
    for (id, path) in &files {
        let timer = mgr.get_timer(id).await.expect("read");
        if *id != "gone" && !path.exists() {
            assert!(timer.is_none(), "{id}: the timer goes with its file");
            deleted.push(id);
        } else {
            assert!(timer.is_some(), "{id}");
        }
    }
    deleted.sort_unstable();
    assert_eq!(deleted, expected);
}

/// No series timer, no series folder, or a series timer that is not stored
/// leaves everything alone.
#[rstest::rstest]
#[case::not_a_series_timer(None, true)]
#[case::no_series_folder(Some(SERIES_TIMER), false)]
#[case::unknown_series_timer(Some("unknown"), true)]
#[tokio::test]
async fn keep_up_to_needs_its_series_timer_and_folder(
    #[case] series_timer_id: Option<&str>,
    #[case] has_folder: bool,
) {
    let dir = tempfile::tempdir().expect("directory");
    let mgr = manager_with_dvr(dir.path()).await;
    let series = dir.path().join("Show");
    std::fs::create_dir_all(&series).expect("series");
    series_timer(&mgr, 1).await;
    let old = completed(&mgr, &series, "old", SERIES_TIMER, 60, true).await;

    mgr.enforce_keep_up_to(series_timer_id, has_folder.then_some(series.as_path()))
        .await
        .expect("enforced");

    assert!(old.exists());
    assert!(mgr.get_timer("old").await.expect("read").is_some());
}

/// With the recordings in the library, a finished timer's file goes through
/// its item; then the series folder's files past the limit go too, newest
/// `DateCreated` kept, whatever recorded them. A file being recorded is never
/// deleted, even past the limit, and an item whose path is a folder is no file.
#[tokio::test]
async fn keep_up_to_deletes_through_the_library() {
    let dir = tempfile::tempdir().expect("directory");
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let series = dir.path().join("Series").join("Show");
    let season = series.join("Season 1");
    std::fs::create_dir_all(&season).expect("season");
    series_timer(&mgr, 2).await;
    let series_item = library.add_item(&series, None, true);
    let season_item = library.add_item(&season, Some(series_item), true);
    // Created oldest first.
    let capturing = season.join("capturing.ts");
    std::fs::write(&capturing, b"recording").expect("capture");
    library.add_item(&capturing, Some(season_item), false);
    mgr.active_recordings_lock().insert(
        "capturing".to_owned(),
        crate::dvr::ActiveRecording::new("capturing".to_owned(), capturing.clone()),
    );
    // A video stored as a folder (a disc) is not a file `File.Exists` sees.
    let disc = season.join("disc");
    std::fs::create_dir_all(&disc).expect("disc");
    library.add_item(&disc, Some(season_item), false);
    let mut taped = Vec::new();
    for name in ["taped-a", "taped-b"] {
        let path = season.join(format!("{name}.ts"));
        std::fs::write(&path, b"taped").expect("taped");
        library.add_item(&path, Some(season_item), false);
        taped.push(path);
    }
    let older = completed(&mgr, &season, "older", SERIES_TIMER, 120, true).await;
    library.add_item(&older, Some(season_item), false);
    let recent = completed(&mgr, &season, "recent", SERIES_TIMER, 60, true).await;
    library.add_item(&recent, Some(season_item), false);

    mgr.enforce_keep_up_to(Some(SERIES_TIMER), Some(&series))
        .await
        .expect("enforced");

    assert!(mgr.get_timer("older").await.expect("read").is_none());
    assert!(mgr.get_timer("recent").await.expect("read").is_some());
    let kept = [&series, &season, &capturing, &disc, &recent].map(|p| p.display().to_string());
    assert_eq!(library.item_paths(), kept);
    for path in [&older, &taped[0], &taped[1]] {
        assert!(!path.exists(), "{}", path.display());
    }
    assert_eq!(recorded_paths(&mgr).await, [recent.display().to_string()]);
}

/// A finished recording's file that a capture is writing again is never
/// deleted, though it is past the limit.
#[tokio::test]
async fn keep_up_to_never_deletes_a_file_being_recorded() {
    let dir = tempfile::tempdir().expect("directory");
    let mgr = manager_with_dvr(dir.path()).await;
    let series = dir.path().join("Show");
    std::fs::create_dir_all(&series).expect("series");
    series_timer(&mgr, 1).await;
    let old = completed(&mgr, &series, "old", SERIES_TIMER, 60, true).await;
    mgr.active_recordings_lock().insert(
        "capturing".to_owned(),
        crate::dvr::ActiveRecording::new("capturing".to_owned(), old.clone()),
    );

    mgr.enforce_keep_up_to(Some(SERIES_TIMER), Some(&series))
        .await
        .expect("enforced");

    assert!(old.exists());
    assert!(mgr.get_timer("old").await.expect("read").is_some());
}

/// A finished recording the library holds no item for goes as a file, with
/// its timer (`DeleteLibraryItemForTimer`'s else branch).
#[tokio::test]
async fn keep_up_to_deletes_a_file_the_library_does_not_hold() {
    let dir = tempfile::tempdir().expect("directory");
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let series = dir.path().join("Show");
    std::fs::create_dir_all(&series).expect("series");
    series_timer(&mgr, 1).await;
    let old = completed(&mgr, &series, "old", SERIES_TIMER, 60, true).await;

    mgr.enforce_keep_up_to(Some(SERIES_TIMER), Some(&series))
        .await
        .expect("enforced");

    assert!(!old.exists());
    assert!(mgr.get_timer("old").await.expect("read").is_none());
    let rows = recorded_paths(&mgr).await;
    assert!(rows.is_empty(), "{rows:?}");
}
