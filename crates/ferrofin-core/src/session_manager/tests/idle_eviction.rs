//! The idle-session sweep of the one-minute loop (gl-1267): a session with no
//! live socket, nothing playing and no request for longer than the idle window
//! ends the way a closed socket ends it, and its device's capabilities are
//! forgotten when no `Devices` row and no other session still uses that device
//! id. A divergence from v12.4, which keeps a socketless session until the
//! server restarts.

use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::PermissionKind;
use ferrofin_model::session::ClientCapabilities;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::WebSocketConnection;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::SessionManager;
use ferrofin_traits::session_bus::SessionMessageBus;
use uuid::Uuid;

use crate::event_manager::EventFuture;

use super::{
    FakeConnection, FerrofinSessionManager, active_at, manager_with_events, playing_session,
    record_event, seed_named_user, set_permission, sign_in, test_db,
};

const WINDOW: TimeDelta = TimeDelta::hours(24);

/// Opens `client`'s session on `device` for `user` and reports capabilities
/// that support remote control, returning the session id.
async fn remote_session(
    mgr: &FerrofinSessionManager,
    user: &UserEntity,
    client: &str,
    device: &str,
) -> String {
    let id = mgr
        .log_session_activity(client, "1.0", device, "TV", "e", Some(user))
        .await
        .expect("session")
        .id
        .expect("session id");
    mgr.report_capabilities(
        &id,
        &ClientCapabilities {
            supports_media_control: true,
            ..ClientCapabilities::default()
        },
    )
    .await
    .expect("capabilities");
    id
}

/// Attaches an open WebSocket to the session directly.
async fn connect(mgr: &FerrofinSessionManager, session_id: &str) {
    let socket = FakeConnection::new(AuthorizationInfo::default());
    mgr.add_web_socket(session_id, socket as Arc<dyn WebSocketConnection>)
        .await
        .expect("socket");
}

#[tokio::test]
async fn an_idle_socketless_session_ends_and_capabilities_no_one_uses_go() {
    let db = test_db().await;
    let (mgr, events) = manager_with_events(&db);
    // The `/socket` handler's path: a sink on the session bus.
    let bus: Arc<dyn SessionMessageBus> = Arc::new(crate::FerrofinSessionMessageBus::new());
    let mgr = Arc::new(mgr.as_ref().clone().with_session_bus(Arc::clone(&bus)));
    let ended = record_event(&events, "SessionEnded");
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "alice").await;
    set_permission(&db, &user.id, PermissionKind::EnableAllDevices, true)
        .await
        .expect("permission");

    // Two clients HTTP only, on a device id no `Devices` row has.
    let idle = remote_session(&mgr, &user, "Kodi", "dev-idle").await;
    let idle_too = remote_session(&mgr, &user, "Infuse", "dev-idle").await;
    // Connected over a WebSocket, as the `/socket` handler connects it.
    let socket = remote_session(&mgr, &user, "Web", "dev-socket").await;
    bus.register(socket.clone(), Box::new(|_| {}));
    // HTTP only, on a device id a directly connected session also uses.
    let shared = remote_session(&mgr, &user, "Kodi", "dev-shared").await;
    let shared_socket = remote_session(&mgr, &user, "Web", "dev-shared").await;
    connect(&mgr, &shared_socket).await;
    // HTTP only, on a device id a sign-in registered (its own session, from
    // the sign-in, is HTTP only too).
    sign_in(&mgr, user_id, "dev-registered").await;
    let registered = remote_session(&mgr, &user, "Kodi", "dev-registered").await;
    // HTTP only, but playing.
    let (playing, _, _) = playing_session(&db, &mgr, "dev-playing", Some(1), false).await;

    let listed = |mgr: Arc<FerrofinSessionManager>| async move {
        let mut ids: Vec<String> = mgr
            .get_sessions(Uuid::nil(), None, None, None, true)
            .await
            .expect("sessions")
            .into_iter()
            .filter_map(|s| s.id)
            .collect();
        ids.sort();
        ids
    };
    let all = listed(Arc::clone(&mgr)).await;
    assert_eq!(all.len(), 8, "{all:?}");
    let last_request = Utc::now();
    for id in &all {
        active_at(&mgr, id, last_request).await;
    }

    let remote_control = |device: &'static str| {
        let devices = Arc::clone(&mgr.device_manager);
        async move {
            devices
                .get_capabilities(Some(device))
                .await
                .expect("capabilities")
                .supports_media_control
        }
    };
    assert!(
        remote_control("dev-idle").await,
        "recorded before the sweep"
    );

    // Idle for exactly the window: nothing ends.
    mgr.run_playback_checks_at(1, 0, WINDOW, last_request + WINDOW)
        .await;
    assert_eq!(listed(Arc::clone(&mgr)).await, all);
    assert!(ended.lock().expect("events").is_empty());

    // Past it: every socketless session that plays nothing ends.
    mgr.run_playback_checks_at(1, 0, WINDOW, last_request + WINDOW + TimeDelta::seconds(1))
        .await;
    let mut kept = vec![socket, shared_socket, playing];
    kept.sort();
    assert_eq!(listed(Arc::clone(&mgr)).await, kept);

    // Each ended through the socket-close path, which publishes `SessionEnded`
    // (the activity log's "has disconnected" entry) once per session.
    let mut ended_ids: Vec<String> = ended
        .lock()
        .expect("events")
        .iter()
        .map(|payload| payload["Id"].as_str().expect("id").to_owned())
        .collect();
    ended_ids.sort();
    let mut expected: Vec<String> = all.into_iter().filter(|id| !kept.contains(id)).collect();
    expected.sort();
    assert_eq!(ended_ids.len(), 5, "{ended_ids:?}");
    assert_eq!(ended_ids, expected);
    for id in [&idle, &idle_too, &shared, &registered] {
        assert!(expected.contains(id), "{id} ended");
    }

    // Only the device id nothing else uses forgets its capabilities.
    assert!(!remote_control("dev-idle").await, "no row, no session");
    assert!(remote_control("dev-shared").await, "a connected session");
    assert!(remote_control("dev-registered").await, "a Devices row");
    assert!(remote_control("dev-socket").await, "its session stays");
}

/// A session that becomes active again while the sweep ends the ones before it
/// is kept: each is checked again just before it ends, and only the devices of
/// the sessions it ended forget their capabilities.
#[tokio::test]
async fn a_session_active_again_during_the_sweep_is_kept() {
    let db = test_db().await;
    let (mgr, events) = manager_with_events(&db);
    let ended = record_event(&events, "SessionEnded");
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let first = remote_session(&mgr, &user, "Kodi", "dev-first").await;
    let second = remote_session(&mgr, &user, "Kodi", "dev-second").await;
    let last_request = Utc::now();
    for id in [&first, &second] {
        active_at(&mgr, id, last_request).await;
    }
    let tick = last_request + WINDOW + TimeDelta::seconds(1);
    // Whichever ends first makes the other active again, as a request landing
    // while its `SessionEnded` is consumed would.
    let pool = Arc::clone(&mgr.sessions);
    events.subscribe(
        "SessionEnded",
        Arc::new(move |_: &str| {
            let pool = Arc::clone(&pool);
            Box::pin(async move {
                for session in pool.lock().await.values_mut() {
                    session.last_activity_date = tick;
                }
                Ok::<(), ServiceError>(())
            }) as EventFuture
        }),
    );

    mgr.run_playback_checks_at(1, 0, WINDOW, tick).await;

    let ended: Vec<String> = ended
        .lock()
        .expect("events")
        .iter()
        .map(|payload| payload["Id"].as_str().expect("id").to_owned())
        .collect();
    assert_eq!(ended.len(), 1, "{ended:?}");
    let ((kept, kept_device), ended_device) = if ended[0] == first {
        ((second, "dev-second"), "dev-first")
    } else {
        ((first, "dev-first"), "dev-second")
    };
    let listed: Vec<String> = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .expect("sessions")
        .into_iter()
        .filter_map(|s| s.id)
        .collect();
    assert_eq!(listed, [kept]);
    let remote_control = |device: &'static str| {
        let devices = Arc::clone(&mgr.device_manager);
        async move {
            devices
                .get_capabilities(Some(device))
                .await
                .expect("capabilities")
                .supports_media_control
        }
    };
    assert!(
        !remote_control(ended_device).await,
        "the ended session's device"
    );
    assert!(
        remote_control(kept_device).await,
        "the kept session's device"
    );
}
