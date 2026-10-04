//! Pins the **legacy user-scoped route aliases** to the router.
//!
//! Upstream Jellyfin still serves ~30 `/Users/{userId}/…` routes it marks
//! `[Obsolete]` + `[ApiExplorerSettings(IgnoreApi = true)]` — hidden from the
//! OpenAPI document, so they are absent from the vendored contract and
//! invisible to the `contract_superset` gate. jellyfin-web's bundled
//! `jellyfin-apiclient` (and many third-party clients) still call them, and a
//! `404` breaks those screens.
//!
//! Each alias must be registered: protected routes return `401` with rejecting
//! auth. Public avatar reads use a nil user id and must return `400`, proving
//! the handler ran rather than returning a route-level `404` or `405`.

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
        let public_image = matches!(*method, "GET" | "HEAD") && path.contains("/Images/");
        let uri = if public_image {
            path.replace(
                "11111111-1111-1111-1111-111111111111",
                "00000000-0000-0000-0000-000000000000",
            )
        } else {
            (*path).to_owned()
        };
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.parse::<Method>().expect("method"))
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if public_image {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{method} {path} must reach its registered handler or auth guard"
        );
    }
}
