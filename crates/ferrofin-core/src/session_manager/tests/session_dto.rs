//! Session DTO members v12.4's `SessionInfoDto` sends whatever the session
//! plays: `LastPlaybackCheckIn` is .NET's `DateTime.MinValue` until a playback
//! report stamps it (`SessionInfo.LastPlaybackCheckIn`, written only by
//! `UpdateNowPlayingItem(..., updateLastCheckInTime: true)`), and
//! `NowPlayingQueueFullItems`, a 10.11 member 12.x dropped, is never sent.

use chrono::Utc;
use ferrofin_model::session::PlaybackStartInfo;
use ferrofin_traits::session::SessionManager;
use uuid::Uuid;

use super::{manager_with_events, record_event, seed_named_user, test_db};

#[tokio::test]
async fn the_check_in_is_the_minimum_date_until_a_playback_report() {
    let db = test_db().await;
    let (mgr, events) = manager_with_events(&db);
    let stopped = record_event(&events, "PlaybackStopped");
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let signed_in = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
        .await
        .expect("session");

    let json = serde_json::to_value(&signed_in).expect("json");
    assert_eq!(json["LastPlaybackCheckIn"], "0001-01-01T00:00:00.0000000Z");
    assert!(
        json.get("NowPlayingQueueFullItems").is_none(),
        "12.x has no NowPlayingQueueFullItems: {json}"
    );

    // The idle check looks only at sessions playing something, so a session
    // whose check-in is the minimum date but never played is not stopped.
    mgr.check_for_idle_playback(Utc::now()).await;
    assert!(stopped.lock().expect("events").is_empty());

    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: signed_in.id.clone(),
        item_id: Uuid::nil(),
        ..PlaybackStartInfo::default()
    })
    .await
    .expect("start");
    let listed = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .expect("sessions");
    assert!(
        listed[0].last_playback_check_in >= signed_in.last_activity_date,
        "the start stamps the check-in"
    );
    let json = serde_json::to_value(&listed[0]).expect("json");
    assert!(json.get("NowPlayingQueueFullItems").is_none(), "{json}");
}
