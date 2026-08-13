//! Pins the **legacy user-scoped route aliases** to the router.
//!
//! Upstream Jellyfin still serves ~30 `/Users/{userId}/…` routes it marks
//! `[Obsolete]` + `[ApiExplorerSettings(IgnoreApi = true)]` — hidden from the
//! OpenAPI document, so they are absent from the vendored contract and
//! invisible to the `contract_superset` gate. jellyfin-web's bundled
//! `jellyfin-apiclient` (and many third-party clients) still call them, and a
//! `404` breaks those screens.
//!
//! Each alias must be *registered*: with the default rejecting auth fake every
//! call returns `401` (route exists, guarded), never `404` (unregistered) or
//! `405` (method not registered on the slot).

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use ferrofin_api::create_router;
use tower::ServiceExt;

/// Every legacy alias Ferrofin serves, as (method, path-with-sample-ids).
const LEGACY_ROUTES: &[(&str, &str)] = &[
    ("GET", "/Users/11111111-1111-1111-1111-111111111111/Items"),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/Resume",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/Latest",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/Root",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/Intros",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/LocalTrailers",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/SpecialFeatures",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/UserData",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/UserData",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/FavoriteItems/22222222-2222-2222-2222-222222222222",
    ),
    (
        "DELETE",
        "/Users/11111111-1111-1111-1111-111111111111/FavoriteItems/22222222-2222-2222-2222-222222222222",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/Rating",
    ),
    (
        "DELETE",
        "/Users/11111111-1111-1111-1111-111111111111/Items/22222222-2222-2222-2222-222222222222/Rating",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/PlayedItems/22222222-2222-2222-2222-222222222222",
    ),
    (
        "DELETE",
        "/Users/11111111-1111-1111-1111-111111111111/PlayedItems/22222222-2222-2222-2222-222222222222",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/PlayingItems/22222222-2222-2222-2222-222222222222",
    ),
    (
        "DELETE",
        "/Users/11111111-1111-1111-1111-111111111111/PlayingItems/22222222-2222-2222-2222-222222222222",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/PlayingItems/22222222-2222-2222-2222-222222222222/Progress",
    ),
    ("GET", "/Users/11111111-1111-1111-1111-111111111111/Views"),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/GroupingOptions",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Suggestions",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Images/Primary",
    ),
    (
        "HEAD",
        "/Users/11111111-1111-1111-1111-111111111111/Images/Primary",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/Images/Primary",
    ),
    (
        "DELETE",
        "/Users/11111111-1111-1111-1111-111111111111/Images/Primary",
    ),
    (
        "GET",
        "/Users/11111111-1111-1111-1111-111111111111/Images/Primary/0",
    ),
    ("POST", "/Users/11111111-1111-1111-1111-111111111111"),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/Password",
    ),
    (
        "POST",
        "/Users/11111111-1111-1111-1111-111111111111/Configuration",
    ),
];

#[tokio::test]
async fn legacy_user_scoped_aliases_are_registered() {
    let router = create_router(ferrofin_api::test_support::fake_state());
    for (method, path) in LEGACY_ROUTES {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.parse::<Method>().expect("method"))
                    .uri(*path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} must be a registered, auth-guarded route (404 = unregistered, 405 = method missing)"
        );
    }
}
