//! Unit 5 client-contract gate: the playback/session/sync/devices controllers
//! are registered but **not yet ported**, so every one of their routes must
//! resolve to `501 Not Implemented` — never `404`.
//!
//! These ~61 operations span the `Session`, `Playstate`, `SyncPlay`, `Devices`,
//! `QuickConnect`, `ApiKey`, and `DisplayPreferences` Jellyfin controllers. A
//! real client (Wolphin) probing any of them must learn "route exists, not
//! implemented", not "no such route". The auth-context middleware is
//! non-rejecting and the shared `not_implemented` stub takes no auth extractor,
//! so a tokenless probe reaches the stub and yields `501` deterministically.
//!
//! The full superset gate lives in `contract_superset.rs`; this test names the
//! Unit-5 tags explicitly so a regression in any of them fails with a clear
//! signal.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use hermit_api::create_router;
use hermit_api::test_support::fake_state;
use tower::ServiceExt;

/// A representative `(method, path)` from each Unit-5 controller. Paths use
/// concrete segment values where the vendored route has a `{param}`.
const UNIT5_PROBES: &[(Method, &str)] = &[
    // SessionController
    (Method::GET, "/Sessions"),
    (Method::POST, "/Sessions/Capabilities"),
    (Method::POST, "/Sessions/Logout"),
    (Method::POST, "/Sessions/Viewing"),
    (Method::POST, "/Sessions/session-1/Command"),
    (Method::POST, "/Sessions/session-1/Command/Mute"),
    (Method::POST, "/Sessions/session-1/Message"),
    (Method::POST, "/Sessions/session-1/Playing"),
    (Method::POST, "/Sessions/session-1/Playing/Pause"),
    (Method::POST, "/Sessions/session-1/System/Restart"),
    (Method::POST, "/Sessions/session-1/User/user-1"),
    (Method::DELETE, "/Sessions/session-1/User/user-1"),
    // PlaystateController
    (Method::POST, "/Sessions/Playing"),
    (Method::POST, "/Sessions/Playing/Ping"),
    (Method::POST, "/Sessions/Playing/Progress"),
    (Method::POST, "/Sessions/Playing/Stopped"),
    (Method::POST, "/PlayingItems/item-1"),
    (Method::DELETE, "/PlayingItems/item-1"),
    (Method::POST, "/PlayingItems/item-1/Progress"),
    (Method::POST, "/UserPlayedItems/item-1"),
    (Method::DELETE, "/UserPlayedItems/item-1"),
    (Method::GET, "/UserItems/item-1/UserData"),
    (Method::POST, "/UserItems/item-1/UserData"),
    // SyncPlayController
    (Method::GET, "/SyncPlay/List"),
    (Method::GET, "/SyncPlay/group-1"),
    (Method::POST, "/SyncPlay/New"),
    (Method::POST, "/SyncPlay/Join"),
    (Method::POST, "/SyncPlay/Leave"),
    (Method::POST, "/SyncPlay/Unpause"),
    (Method::POST, "/SyncPlay/Pause"),
    (Method::POST, "/SyncPlay/Seek"),
    (Method::POST, "/SyncPlay/SetNewQueue"),
    (Method::POST, "/SyncPlay/SetRepeatMode"),
    (Method::POST, "/SyncPlay/SetShuffleMode"),
    // DevicesController
    (Method::GET, "/Devices"),
    (Method::DELETE, "/Devices"),
    (Method::GET, "/Devices/Info"),
    (Method::GET, "/Devices/Options"),
    (Method::POST, "/Devices/Options"),
    // QuickConnectController
    (Method::GET, "/QuickConnect/Enabled"),
    (Method::GET, "/QuickConnect/Connect"),
    (Method::POST, "/QuickConnect/Initiate"),
    (Method::POST, "/QuickConnect/Authorize"),
    // ApiKeyController
    (Method::GET, "/Auth/Keys"),
    (Method::POST, "/Auth/Keys"),
    (Method::DELETE, "/Auth/Keys/some-key"),
    (Method::GET, "/Auth/Providers"),
    (Method::GET, "/Auth/PasswordResetProviders"),
    // DisplayPreferencesController
    (Method::GET, "/DisplayPreferences/usersettings"),
    (Method::POST, "/DisplayPreferences/usersettings"),
];

#[tokio::test]
async fn unit5_stub_routes_return_501_not_404() {
    let router = create_router(fake_state());

    for (method, uri) in UNIT5_PROBES {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(*uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "Unit-5 contract route {method} {uri} must be a registered 501 stub, got {}",
            response.status()
        );
    }
}
