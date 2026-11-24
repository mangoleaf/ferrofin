//! Adoption's import of Jellyfin's timers and series timers
//! (`data/livetv/timers.json`, `seriestimers.json`).

use std::collections::HashMap;
use std::path::Path;

use ferrofin_core::config_export::{
    LIVE_TV_SERIES_TIMERS_FILE, LIVE_TV_TIMERS_FILE, export_live_tv,
};
use ferrofin_db::export::{CarriedRows, PathRewrite, SeriesTimerRow, TimerRow};
use ferrofin_model::live_tv::{RecordingQuery, RecordingStatus};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::{LiveTvManager as _, internal_timer_id};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{FakeFetcher, manager_with, manager_with_relative_guide};
use crate::manager::{
    FerrofinLiveTvManager, ImportedTimers, LiveTvPaths, import_jellyfin_timers, parse_dt,
};
use crate::{dvr_repository, guide_repository};

/// What Jellyfin 12.4 wrote for a film timer and a series timer with its six
/// airings (the export's golden files).
const GOLDEN_TIMERS: &str =
    include_str!("../../../../ferrofin-core/src/config_export/testdata/timers.json");
const GOLDEN_SERIES: &str =
    include_str!("../../../../ferrofin-core/src/config_export/testdata/seriestimers.json");

/// The import's `FerrofinMeta` key.
const IMPORTED: &str = "livetv_timers_imported_v1";

/// `mgr` with `root/data` as its data directory, holding `timers` and
/// `series` as Jellyfin keeps them, when given.
fn over(
    mgr: FerrofinLiveTvManager,
    root: &Path,
    timers: Option<&str>,
    series: Option<&str>,
) -> FerrofinLiveTvManager {
    let dir = root.join("data/livetv");
    std::fs::create_dir_all(&dir).expect("livetv dir");
    for (file, contents) in [("timers.json", timers), ("seriestimers.json", series)] {
        if let Some(contents) = contents {
            std::fs::write(dir.join(file), contents).expect("write");
        }
    }
    mgr.with_paths(LiveTvPaths {
        data_dir: root.join("data"),
        ..LiveTvPaths::default()
    })
}

/// A manager with no guide over `root`, holding `timers` and `series`.
async fn manager(root: &Path, timers: Option<&str>, series: Option<&str>) -> FerrofinLiveTvManager {
    over(
        manager_with(FakeFetcher(HashMap::new())).await,
        root,
        timers,
        series,
    )
}

/// The import over `mgr`'s database and data directory, as the boot runs it.
async fn import(mgr: &FerrofinLiveTvManager) -> Result<ImportedTimers, ServiceError> {
    import_jellyfin_timers(&mgr.db, &mgr.server_id, &mgr.paths.data_dir).await
}

/// Golden's film timer, alone.
fn film() -> String {
    let golden: Vec<Value> = serde_json::from_str(GOLDEN_TIMERS).expect("json");
    json!([golden[0]]).to_string()
}

/// `timer` as the export writes a scheduled one: the programme facts a guide
/// refresh copies into it at their defaults.
fn scheduled(mut timer: Value) -> Value {
    let fields = timer.as_object_mut().expect("object");
    for key in [
        "ShowId",
        "SeriesId",
        "SeasonNumber",
        "EpisodeNumber",
        "ProductionYear",
        "EpisodeTitle",
    ] {
        fields.remove(key);
    }
    fields.insert("ProviderIds".to_owned(), json!({}));
    fields.insert("Tags".to_owned(), json!([]));
    for key in ["IsMovie", "IsSeries", "IsProgramSeries"] {
        fields.insert(key.to_owned(), json!(false));
    }
    timer
}

/// `list` in `Id` order.
fn by_id(mut list: Vec<Value>) -> Vec<Value> {
    list.sort_by_key(|entry| entry["Id"].as_str().map(str::to_owned));
    list
}

/// What Jellyfin wrote is what the export writes back from the rows the
/// import stored: every id, date, status, padding, priority, `IsManual` and
/// series rule, but for the programme facts the export writes as their
/// defaults on a scheduled timer.
#[tokio::test]
async fn jellyfins_files_go_back_out_as_jellyfin_wrote_them() {
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager(root.path(), Some(GOLDEN_TIMERS), Some(GOLDEN_SERIES)).await;
    assert_eq!(
        import(&mgr).await.expect("import"),
        ImportedTimers {
            timers: 7,
            recordings: 0,
            series_timers: 1,
        }
    );

    let mut carried = CarriedRows::default();
    for timer in mgr.all_timers().await.expect("timers") {
        let id = timer.base.id.clone().expect("id");
        carried.timers.push(TimerRow {
            is_manual: dvr_repository::timer_is_manual(&mgr.db, &id)
                .await
                .expect("manual"),
            id,
            data: serde_json::to_string(&timer).expect("serialize"),
            recording: None,
        });
    }
    for (id, data, series_id) in dvr_repository::series_timer_rows(&mgr.db)
        .await
        .expect("series timers")
    {
        let (_, external_id, _) = dvr_repository::series_timer_row(&mgr.db, &id)
            .await
            .expect("read")
            .expect("stored");
        carried.series_timers.push(SeriesTimerRow {
            data,
            external_id,
            series_id,
        });
    }
    let config = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(config.path().join("users/named")).expect("named");
    std::fs::write(config.path().join("users/named/livetv.json"), "{}").expect("options");
    let export = export_live_tv(config.path(), &carried, &PathRewrite::default()).expect("exports");
    let exported = |path: &str| -> Vec<Value> {
        let file = export.files.files.iter().find(|f| f.path == path);
        serde_json::from_str(&file.expect("exported").contents).expect("json")
    };

    let golden: Vec<Value> = serde_json::from_str(GOLDEN_TIMERS).expect("json");
    assert_eq!(
        by_id(exported(LIVE_TV_TIMERS_FILE)),
        by_id(golden.into_iter().map(scheduled).collect())
    );
    let golden: Vec<Value> = serde_json::from_str(GOLDEN_SERIES).expect("json");
    assert_eq!(exported(LIVE_TV_SERIES_TIMERS_FILE), golden);
}

/// The ids the import derives are the guide's: the first guide refresh keeps
/// an imported timer whose programme it lists and deletes one whose programme
/// it does not, and a series timer's fan-out finds the airing's timer Jellyfin
/// made instead of scheduling the airing again.
#[tokio::test]
async fn the_guide_refresh_finds_what_the_import_stored() {
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager_with_relative_guide().await;
    let later = guide_repository::test_support::program_rows(&mgr.db)
        .await
        .expect("programmes")
        .into_iter()
        .find(|p| p.title == "Later")
        .expect("the later airing");
    let channel_id = Uuid::parse_str(&later.channel_id).expect("channel id");
    let channel = guide_repository::channel_external_id(&mgr.db, channel_id)
        .await
        .expect("read")
        .expect("its channel");
    let programme = later.external_id.clone().expect("listing id");
    let date =
        |text: &str| ferrofin_model::json::datetime::format(&parse_dt(text).expect("stored date"));
    let (start, end) = (
        date(&later.start_date),
        date(later.end_date.as_deref().expect("end")),
    );
    let series = "8a80a5bd59324c698d56c12a9a8ba0e1";
    let airing = ferrofin_common::extensions::get_md5(&format!("{series}{programme}"))
        .simple()
        .to_string();
    let timers = json!([
        {"Id": airing, "SeriesTimerId": series, "ChannelId": channel, "ProgramId": programme,
         "Name": "Later", "StartDate": start, "EndDate": end, "Status": "New"},
        {"Id": "gone", "ChannelId": channel, "ProgramId": "gone.tv_2026", "Name": "Gone",
         "StartDate": start, "EndDate": end, "Status": "New", "IsManual": true},
    ]);
    let series_timers = json!([
        {"Id": series, "ChannelId": channel, "ProgramId": programme, "Name": "Later",
         "StartDate": start, "EndDate": end, "RecordAnyTime": true, "RecordNewOnly": false,
         "SkipEpisodesInLibrary": false,
         "Days": ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"]},
    ]);
    let mgr = over(
        mgr,
        root.path(),
        Some(&timers.to_string()),
        Some(&series_timers.to_string()),
    );
    import(&mgr).await.expect("import");
    mgr.refresh_guide().await.expect("refresh");

    let kept: Vec<_> = mgr
        .get_timers()
        .await
        .expect("timers")
        .into_iter()
        .map(|timer| (timer.base.id, timer.base.channel_id))
        .collect();
    assert_eq!(kept, [(Some(internal_timer_id(&airing)), channel_id)]);
}

/// The import runs once, on a database's first boot: a timer removed after
/// it stays removed, and a database a Ferrofin booted before (its Live TV
/// configuration import has run) imports nothing, unless an import a failure
/// left owed is still to run.
#[tokio::test]
async fn the_import_runs_once_on_the_first_boot() {
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager(root.path(), Some(&film()), None).await;
    assert_eq!(import(&mgr).await.expect("import").timers, 1);
    let id = internal_timer_id("f5301b18ffda4b789f7335bca4545543");
    assert!(mgr.get_timer(&id).await.expect("read").is_some());
    mgr.cancel_timer(&id).await.expect("the admin removes it");
    assert_eq!(
        import(&mgr).await.expect("import"),
        ImportedTimers::default()
    );
    assert!(mgr.get_timer(&id).await.expect("read").is_none());

    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager(root.path(), Some(&film()), None).await;
    ferrofin_core::live_tv_import::import_live_tv_config(&mgr.db, root.path())
        .await
        .expect("a boot before");
    assert_eq!(
        import(&mgr).await.expect("import"),
        ImportedTimers::default()
    );
    assert!(mgr.get_timer(&id).await.expect("read").is_none());
    assert_eq!(
        mgr.db.meta_get(IMPORTED).await.expect("meta").as_deref(),
        Some("1")
    );

    mgr.db.meta_set(IMPORTED, "pending").await.expect("owed");
    assert_eq!(import(&mgr).await.expect("import").timers, 1);
    assert!(mgr.get_timer(&id).await.expect("read").is_some());
}

/// A file that does not parse imports none of its entries, as Jellyfin reads
/// it as empty, and is not read again; the other file still imports.
#[tokio::test]
async fn a_file_that_does_not_parse_imports_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager(root.path(), Some("[{"), Some(GOLDEN_SERIES)).await;
    assert_eq!(
        import(&mgr).await.expect("import"),
        ImportedTimers {
            timers: 0,
            recordings: 0,
            series_timers: 1,
        }
    );
    assert_eq!(mgr.all_timers().await.expect("timers"), []);
    assert_eq!(
        mgr.db.meta_get(IMPORTED).await.expect("meta").as_deref(),
        Some("1")
    );
}

/// A completed timer keeps its file as a recording row, with the programme
/// facts its entry kept, and stays out of the scheduled list; a retried
/// import writes the row over, not beside, the first.
#[tokio::test]
async fn a_completed_timer_keeps_its_recording() {
    let root = tempfile::tempdir().expect("tempdir");
    let file = root.path().join("Quiz Night S01E02.ts");
    std::fs::write(&file, b"recorded").expect("recording");
    let timers = json!([{
        "Id": "c0ffee", "SeriesTimerId": "8a80a5bd59324c698d56c12a9a8ba0e1",
        "ChannelId": "m3u_1", "ProgramId": "quiz.tv_1", "Name": "Quiz Night",
        "StartDate": "2026-11-22T21:00:00.0000000Z", "EndDate": "2026-11-22T22:00:00.0000000Z",
        "Status": "Completed", "SeasonNumber": 1, "EpisodeNumber": 2, "EpisodeTitle": "Round 2",
        "IsProgramSeries": true, "Tags": ["kids", "Series"], "RecordingPath": file,
    }]);
    let mgr = manager(root.path(), Some(&timers.to_string()), None).await;
    assert_eq!(
        import(&mgr).await.expect("import"),
        ImportedTimers {
            timers: 1,
            recordings: 1,
            series_timers: 0,
        }
    );
    let id = internal_timer_id("c0ffee");
    assert_eq!(mgr.get_timers().await.expect("timers"), []);
    let timer = mgr.get_timer(&id).await.expect("read").expect("kept");
    assert_eq!(timer.status, RecordingStatus::Completed);

    mgr.db.meta_set(IMPORTED, "pending").await.expect("owed");
    import(&mgr).await.expect("retried");
    let completed = RecordingQuery {
        status: Some(RecordingStatus::Completed),
        ..RecordingQuery::default()
    };
    let rows = dvr_repository::recording_rows(&mgr.db, &completed)
        .await
        .expect("recordings");
    let [row] = rows.as_slice() else {
        panic!("one recording, got {}", rows.len());
    };
    assert_eq!(row.timer_id.as_deref(), Some(id.as_str()));
    assert_eq!(row.path.as_deref(), file.to_str());
    assert_eq!(row.series_timer_id, timer.series_timer_id);
    assert_eq!(
        (
            row.season_number,
            row.episode_number,
            row.episode_title.as_deref()
        ),
        (Some(1), Some(2), Some("Round 2"))
    );
    assert!(row.is_series && row.is_kids && !row.is_news, "{row:?}");
}

/// The ids a stock Jellyfin 12.4 served for a series timer and one of its
/// timers (`GET /LiveTv/Timers`, `GET /LiveTv/SeriesTimers`) are the ones the
/// import derives from the entries that Jellyfin wrote for them.
#[tokio::test]
async fn the_ids_are_the_ones_jellyfin_serves() {
    const CHANNEL: &str = "m3u_ebd429070cd6bc02cb1db5b387e897f26caac8f23e707316bf32d2fb5d950bd2";
    let programme = format!("news.test_2026-11-23T17:00:00.0000000+00:00_{CHANNEL}");
    let timers = json!([{
        "Id": "f12874e7925744aae3f9776661b31e13",
        "SeriesTimerId": "cc72d8658c36428bac209d995def2989",
        "ChannelId": CHANNEL, "ProgramId": programme, "Name": "Quiz Night",
        "StartDate": "2026-11-23T17:00:00.0000000Z", "EndDate": "2026-11-23T18:00:00.0000000Z",
    }]);
    let series = json!([{
        "Id": "cc72d8658c36428bac209d995def2989", "ChannelId": CHANNEL, "ProgramId": programme,
        "Name": "Quiz Night",
    }]);
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager(
        root.path(),
        Some(&timers.to_string()),
        Some(&series.to_string()),
    )
    .await;
    import(&mgr).await.expect("import");
    let channel = Uuid::parse_str("032104cc77da3632f0c9af5bf26b00be").expect("uuid");
    let timer = mgr
        .get_timer("02023577d6c5790f0402fa8148a99df7")
        .await
        .expect("read")
        .expect("stored under Jellyfin's id");
    assert_eq!(
        (
            timer.base.channel_id,
            timer.base.program_id.as_deref(),
            timer.series_timer_id.as_deref()
        ),
        (
            channel,
            Some("37a69168d2a30614d2915d888abef215"),
            Some("637c779634aaa09b56a63e5d06d16736")
        )
    );
    let series = mgr
        .get_series_timer("637c779634aaa09b56a63e5d06d16736")
        .await
        .expect("read")
        .expect("stored under Jellyfin's id");
    assert_eq!(
        (series.base.channel_id, series.base.program_id.as_deref()),
        (channel, Some("37a69168d2a30614d2915d888abef215"))
    );
}

/// A file that is there but cannot be read leaves the import owed to the next
/// boot, which a fixed permission lets it finish.
#[tokio::test]
async fn a_file_that_cannot_be_read_leaves_the_import_owed() {
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = manager(root.path(), None, None).await;
    let timers = root.path().join("data/livetv/timers.json");
    // A directory where the file should be: present, and unreadable as one.
    std::fs::create_dir(&timers).expect("dir");
    import(&mgr)
        .await
        .expect_err("an unreadable file is an error");
    assert_eq!(
        mgr.db.meta_get(IMPORTED).await.expect("meta").as_deref(),
        Some("pending")
    );

    std::fs::remove_dir(&timers).expect("fixed");
    std::fs::write(&timers, film()).expect("write");
    assert_eq!(import(&mgr).await.expect("import").timers, 1);
}
