//! The HTTP layer is a pure pass-through when metrics are never initialised.
//!
//! Its own binary (fresh process): the instrument `OnceLock` is guaranteed
//! unset here, so this proves the disabled path adds nothing.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use tower::ServiceExt as _;

#[tokio::test]
async fn layer_without_init_passes_through() {
    // Note: no `ferrofin_metrics::init` call — the instruments are never installed.
    let app = Router::new()
        .route("/hello", get(|| async { "world" }))
        .layer(axum::middleware::from_fn(ferrofin_metrics::track_http));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"world");
}
