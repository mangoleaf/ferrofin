//! A playback event carries the item its report names as `MediaInfo` (C#
//! `MediaInfo = info.Item`), so the activity loggers read no row of their own
//! (gl-1392). A stop names the item the session showed, as C#
//! `info.Item = current` does, while the library still has it.

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::session::{PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo};
use ferrofin_traits::library::LibraryManager as _;
use ferrofin_traits::persistence::ItemPersistenceService as _;
use ferrofin_traits::session::SessionManager as _;
use rstest::rstest;
use uuid::Uuid;

use super::{library_manager, manager_with_events, record_event, seed_named_user, test_db};

/// A start and a progress report carry the now-playing item. After the row is
/// renamed mid-play, the stop still names it as the session showed it; after it
/// is deleted, the stop carries no item.
#[rstest]
#[case::renamed_mid_play(false, Some("Film"))]
#[case::deleted_mid_play(true, None)]
#[tokio::test]
async fn a_stop_names_the_item_the_session_showed(
    #[case] deleted: bool,
    #[case] stop_name: Option<&str>,
) {
    let db = test_db().await;
    let (mgr, events) = manager_with_events(&db);
    let started = record_event(&events, "PlaybackStart");
    let progressed = record_event(&events, "PlaybackProgress");
    let stopped = record_event(&events, "PlaybackStopped");
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let session_id = mgr
        .log_session_activity("Web", "1.0", "dev-events", "TV", "e", Some(&user))
        .await
        .unwrap()
        .id
        .unwrap();
    let item = Uuid::new_v4();
    crate::test_support::seed_named_item(&db, item, BaseItemKind::Movie, "Film").await;
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: Some(session_id.clone()),
        item_id: item,
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();
    mgr.on_playback_progress(
        &PlaybackProgressInfo {
            session_id: Some(session_id.clone()),
            item_id: item,
            position_ticks: Some(10),
            ..PlaybackProgressInfo::default()
        },
        false,
    )
    .await
    .unwrap();

    if deleted {
        crate::FerrofinItemPersistenceService::new(db.clone())
            .delete_items(&[item])
            .await
            .unwrap();
    } else {
        let library = library_manager(&db);
        let mut row = library.get_item_by_id(item).await.unwrap().expect("row");
        row.name = Some("Film Renamed".to_owned());
        library.update_items(&[row], None).await.unwrap();
        let now = library.get_item_by_id(item).await.unwrap().expect("row");
        assert_eq!(now.name.as_deref(), Some("Film Renamed"));
    }
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: Some(session_id),
        item_id: item,
        position_ticks: Some(30),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();

    assert_eq!(
        started.lock().unwrap()[0]["MediaInfo"],
        serde_json::json!({
            "Name": "Film", "SeriesName": null, "Artists": [], "MediaType": null, "ExtraType": null,
        })
    );
    assert_eq!(progressed.lock().unwrap()[0]["MediaInfo"]["Name"], "Film");
    let stopped = &stopped.lock().unwrap()[0];
    assert_eq!(stopped["MediaInfo"].is_null(), deleted, "{stopped}");
    assert_eq!(
        stopped["MediaInfo"]["Name"].as_str(),
        stop_name,
        "{stopped}"
    );
}

/// A stop naming an item the session does not show names the row it read, with
/// every member the loggers read in its `BaseItemDto` form: the stored artists
/// as a list, and the stored `ExtraType` 8 as `ThemeSong`.
#[tokio::test]
async fn a_stop_of_an_item_not_shown_carries_the_row_it_read() {
    let db = test_db().await;
    let (mgr, events) = manager_with_events(&db);
    let stopped = record_event(&events, "PlaybackStopped");
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let session_id = mgr
        .log_session_activity("Web", "1.0", "dev-not-shown", "TV", "e", Some(&user))
        .await
        .unwrap()
        .id
        .unwrap();
    let item = Uuid::new_v4();
    crate::test_support::seed_named_item(&db, item, BaseItemKind::Audio, "Track").await;
    let library = library_manager(&db);
    let mut row = library.get_item_by_id(item).await.unwrap().expect("row");
    row.series_name = Some("Show".to_owned());
    row.artists = Some("A|B".to_owned());
    row.media_type = Some("Audio".to_owned());
    row.extra_type = Some(8);
    library.update_items(&[row], None).await.unwrap();

    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: Some(session_id),
        item_id: item,
        position_ticks: Some(30),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();

    assert_eq!(
        stopped.lock().unwrap()[0]["MediaInfo"],
        serde_json::json!({
            "Name": "Track", "SeriesName": "Show", "Artists": ["A", "B"], "MediaType": "Audio",
            "ExtraType": "ThemeSong",
        })
    );
}
