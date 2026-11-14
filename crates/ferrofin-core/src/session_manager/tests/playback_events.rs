//! A playback event carries the item its report names as `MediaInfo` (C#
//! `MediaInfo = info.Item`) and the library item as `Item` (C#
//! `eventArgs.Item`), so the activity loggers read no row of their own
//! (gl-1392). A stop names the item the session showed, as C#
//! `info.Item = current` does, while the library still has it. A report's own
//! `Item` is the `MediaInfo` as sent (gl-1062).

use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::BaseItemDto;
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
    assert_eq!(
        started.lock().unwrap()[0]["Item"],
        serde_json::json!({ "Id": item, "ExtraType": null })
    );
    assert_eq!(progressed.lock().unwrap()[0]["MediaInfo"]["Name"], "Film");
    let stopped = &stopped.lock().unwrap()[0];
    assert_eq!(stopped["MediaInfo"].is_null(), deleted, "{stopped}");
    assert_eq!(stopped["Item"].is_null(), deleted, "{stopped}");
    assert_eq!(
        stopped["MediaInfo"]["Name"].as_str(),
        stop_name,
        "{stopped}"
    );
}

/// A stop naming an item the session does not show names the row it read, with
/// every member the loggers read in its `BaseItemDto` form: the stored artists
/// as a list, and the stored `ExtraType` 8 as `ThemeSong`, which the library
/// item carries too.
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

    let stopped = &stopped.lock().unwrap()[0];
    assert_eq!(
        stopped["MediaInfo"],
        serde_json::json!({
            "Name": "Track", "SeriesName": "Show", "Artists": ["A", "B"], "MediaType": "Audio",
            "ExtraType": "ThemeSong",
        })
    );
    assert_eq!(
        stopped["Item"],
        serde_json::json!({ "Id": item, "ExtraType": "ThemeSong" })
    );
}

/// A report's own `Item` is the event's `MediaInfo` as sent, on the start, the
/// progress and the stop; a stop without one names the one the session shows,
/// and the idle stop carries it (C# `Item = session.NowPlayingItem`). `Item` is the library item the report
/// names: none for one the library does not have, its `ExtraType` (C#
/// `IsThemeMedia`) for one it has (gl-1062).
#[tokio::test]
async fn a_reports_own_item_is_the_events_media_info() {
    let db = test_db().await;
    let (mgr, events) = manager_with_events(&db);
    let started = record_event(&events, "PlaybackStart");
    let progressed = record_event(&events, "PlaybackProgress");
    let stopped = record_event(&events, "PlaybackStopped");
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let session_id = mgr
        .log_session_activity("Web", "1.0", "dev-own-item", "TV", "e", Some(&user))
        .await
        .unwrap()
        .id
        .unwrap();
    let external = Uuid::new_v4();
    let broadcast = BaseItemDto {
        id: external,
        name: Some("Broadcast".to_owned()),
        series_name: Some("Channel".to_owned()),
        artists: Some(vec!["Host".to_owned()]),
        media_type: MediaType::Video,
        ..BaseItemDto::default()
    };
    let report = PlaybackProgressInfo {
        session_id: Some(session_id.clone()),
        item_id: external,
        item: Some(broadcast.clone()),
        ..PlaybackProgressInfo::default()
    };
    mgr.on_playback_start(&report).await.unwrap();
    mgr.on_playback_progress(&report, false).await.unwrap();
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: Some(session_id.clone()),
        item_id: external,
        item: Some(broadcast),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    let media_info = serde_json::json!({
        "Name": "Broadcast", "SeriesName": "Channel", "Artists": ["Host"], "MediaType": "Video",
        "ExtraType": null,
    });
    for event in [&started, &progressed, &stopped] {
        let event = &event.lock().unwrap()[0];
        assert_eq!(event["MediaInfo"], media_info, "{event}");
        assert!(event["Item"].is_null(), "{event}");
    }

    // The idle stop of a library item reported with the client's own Item.
    let song = Uuid::new_v4();
    crate::test_support::seed_named_item(&db, song, BaseItemKind::Audio, "Song").await;
    let library = library_manager(&db);
    let mut row = library.get_item_by_id(song).await.unwrap().expect("row");
    row.extra_type = Some(8);
    library.update_items(&[row], None).await.unwrap();
    let start = PlaybackStartInfo {
        session_id: Some(session_id.clone()),
        item_id: song,
        item: Some(BaseItemDto {
            id: song,
            name: Some("Client Title".to_owned()),
            ..BaseItemDto::default()
        }),
        ..PlaybackStartInfo::default()
    };
    // A stop without its own Item names the one the session shows (C#
    // `info.Item = current`), also when it was sent under another ItemId.
    let stop = PlaybackStopInfo {
        session_id: Some(session_id),
        item_id: song,
        ..PlaybackStopInfo::default()
    };
    for item_id in [song, external] {
        mgr.on_playback_start(&PlaybackStartInfo {
            item_id,
            ..start.clone()
        })
        .await
        .unwrap();
        mgr.on_playback_stopped(&stop).await.unwrap();
    }
    for stopped in &stopped.lock().unwrap()[1..3] {
        assert_eq!(stopped["MediaInfo"]["Name"], "Client Title", "{stopped}");
    }
    mgr.on_playback_start(&start).await.unwrap();
    mgr.check_for_idle_playback(chrono::Utc::now() + chrono::Duration::minutes(6))
        .await;
    let idle = &stopped.lock().unwrap()[3];
    assert_eq!(idle["MediaInfo"]["Name"], "Client Title", "{idle}");
    assert_eq!(
        idle["Item"],
        serde_json::json!({ "Id": song, "ExtraType": "ThemeSong" })
    );
}
