//! The stateless `GET /metrics` router.

use axum::Router;
use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use prometheus::{Registry, TextEncoder};

use crate::init::MetricsHandle;

/// The Prometheus text-exposition content type (format version 0.0.4).
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

impl MetricsHandle {
    /// Builds the stateless `GET /metrics` router, ready to `.merge` into the
    /// app router. Each request gathers the registry (running every observable
    /// callback) and renders the exposition.
    pub fn router(&self) -> Router {
        let registry = self.registry();
        Router::new().route("/metrics", get(move || render(registry.clone())))
    }
}

/// Renders the current exposition with the correct content type. `async` is
/// required by axum's handler signature though the body doesn't await.
#[allow(clippy::unused_async)]
async fn render(registry: Registry) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let body = TextEncoder::new()
        .encode_to_string(&registry.gather())
        .unwrap_or_default();
    ([(CONTENT_TYPE, EXPOSITION_CONTENT_TYPE)], body)
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    use crate::init::build_provider;

    #[tokio::test]
    async fn metrics_endpoint_serves_exposition() {
        let (registry, provider) = build_provider().expect("exporter builds");
        let handle = super::MetricsHandle::for_test(registry, provider);
        // A registered counter so the exposition is non-empty.
        let counter = handle.meter().u64_counter("router_probe_total").build();
        counter.add(1, &[]);

        let response = handle
            .router()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            super::EXPOSITION_CONTENT_TYPE
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("router_probe_total"));
    }
}
