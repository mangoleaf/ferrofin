//! Unit 5 client-contract gate: the playback/session/sync/devices controllers
//! are registered but **not yet ported**, so every one of their routes must
//! resolve to `501 Not Implemented` — never `404`.
//!
//! These operations span the `SyncPlay` and `DisplayPreferences` Jellyfin
//! controllers (the `Session`, `Playstate`, and `QuickConnect` controllers were
//! ported in Batches 5–6, and `Devices`/`ApiKey` in Batch 12, so their routes
//! are real now and covered by their own tests). A
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
    // SessionController + PlaystateController were ported in Batch 5 (Playstate +
    // Sessions playback reporting), so their routes are now real (`RequireAuth`-
    // guarded → `401` for a tokenless probe) rather than `501` stubs; likewise
    // `/Auth/Providers` + `/Auth/PasswordResetProviders` (ported alongside the
    // session controller). They are exercised in `session_playstate.rs` instead.
    //
    // Note: `/UserItems/{itemId}/UserData` (GET/POST) was ported in Batch 4
    // (real user-data read/write), so it is no longer a Unit-5 stub.
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
    // DevicesController + ApiKeyController were ported in Batch 12, so their
    // routes are now real (`RequireAuth`-guarded → `401` for a tokenless probe)
    // and exercised in `batch12_handlers.rs` instead of being 501 stubs here.
    // QuickConnectController was ported in Batch 6, so its routes are now real
    // (`/QuickConnect/Enabled`/`Connect`/`Initiate`/`Authorize`); they are
    // exercised in `batch6_handlers.rs` instead of being 501 stubs here.
    // `/Auth/Providers` + `/Auth/PasswordResetProviders` are now real (Batch 5).
    // DisplayPreferencesController was ported in Batch 13, so its
    // `/DisplayPreferences/{displayPreferencesId}` GET/POST routes are now real
    // (`RequireAuth`-guarded) and exercised in `batch13_handlers.rs` instead of
    // being 501 stubs here.
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
